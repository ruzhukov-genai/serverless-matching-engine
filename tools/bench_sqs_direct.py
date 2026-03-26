#!/usr/bin/env python3
"""
AWS Benchmark — SQS-Direct mode.
POST /api/orders → API Gateway → SQS FIFO → Worker Lambda (event source mapping).
No manual drain needed. Fire orders, wait for SQS to drain, query DB lifecycle.
"""
import asyncio, aiohttp, subprocess, json, time, statistics, sys
from datetime import datetime, timezone

API_URL = "https://kpvhsf0ub8.execute-api.us-east-1.amazonaws.com"
WORKER_LAMBDA = "serverless-matching-engine-worker"
SQS_URL = "https://queue.amazonaws.com/210352747749/serverless-matching-engine-orders"
PAIR = "BTC-USDT"
DURATION = 30
NUM_USERS = 100
CLIENT_COUNTS = [1, 10, 50, 100]

def lambda_invoke(payload):
    try:
        subprocess.check_output([
            "aws", "lambda", "invoke", "--function-name", WORKER_LAMBDA,
            "--payload", json.dumps(payload), "--cli-read-timeout", "30",
            "/tmp/_bench_invoke.json"
        ], stderr=subprocess.DEVNULL)
        with open("/tmp/_bench_invoke.json") as f:
            return json.load(f)
    except Exception as e:
        return {"error": str(e)}

def lambda_query(sql):
    resp = lambda_invoke({"manage": {"command": "query", "sql": sql}})
    return resp.get("rows", [])

def sqs_depth():
    try:
        out = subprocess.check_output([
            "aws", "sqs", "get-queue-attributes", "--queue-url", SQS_URL,
            "--attribute-names", "ApproximateNumberOfMessages", "ApproximateNumberOfMessagesNotVisible",
            "--query", "Attributes", "--output", "json"
        ], stderr=subprocess.DEVNULL)
        attrs = json.loads(out)
        return int(attrs.get("ApproximateNumberOfMessages", 0)) + int(attrs.get("ApproximateNumberOfMessagesNotVisible", 0))
    except:
        return -1

async def place_orders(session, num_clients, run_id, deadline):
    placed = 0
    errors = 0
    latencies = []

    async def client_loop(cid):
        nonlocal placed, errors
        seq = 0
        uid = f"user-{(cid % NUM_USERS) + 1}"
        while time.monotonic() < deadline:
            seq += 1
            side = "Buy" if seq % 2 == 0 else "Sell"
            price = f"{69000 + (seq % 2000)}.00"
            order = {
                "pair_id": PAIR, "side": side, "order_type": "Limit",
                "price": price, "quantity": "0.001",
                "user_id": uid,
                "client_order_id": f"{run_id}-c{cid}-{seq}"
            }
            t0 = time.monotonic()
            try:
                async with session.post(f"{API_URL}/api/orders", json=order) as resp:
                    await resp.read()
                    lat = (time.monotonic() - t0) * 1000
                    if resp.status == 200:
                        placed += 1
                        latencies.append(lat)
                    else:
                        errors += 1
            except Exception:
                errors += 1

    tasks = [asyncio.create_task(client_loop(i)) for i in range(num_clients)]
    await asyncio.gather(*tasks)
    return placed, errors, latencies

async def run_level(num_clients, run_id):
    print(f"\n{'='*60}")
    print(f"  SQS-Direct Benchmark: c={num_clients}, duration={DURATION}s")
    print(f"{'='*60}")

    connector = aiohttp.TCPConnector(limit=num_clients + 10, limit_per_host=num_clients + 10)
    async with aiohttp.ClientSession(connector=connector) as session:
        deadline = time.monotonic() + DURATION
        t_start = time.monotonic()
        placed, errors, latencies = await place_orders(session, num_clients, run_id, deadline)
        elapsed = time.monotonic() - t_start

    rate = placed / elapsed if elapsed > 0 else 0
    print(f"\n  Client results:")
    print(f"    Placed: {placed} orders in {elapsed:.1f}s = {rate:.1f} ord/s")
    print(f"    Errors: {errors}")
    if latencies:
        latencies.sort()
        print(f"    Latency: avg={statistics.mean(latencies):.0f}ms  p50={latencies[len(latencies)//2]:.0f}ms  p95={latencies[int(len(latencies)*0.95)]:.0f}ms  p99={latencies[int(len(latencies)*0.99)]:.0f}ms")

    # Report SQS depth (don't wait for drain)
    depth = sqs_depth()
    print(f"\n  SQS depth after dispatch: {depth}")

    return {"clients": num_clients, "placed": placed, "rate": rate, "errors": errors}

async def main():
    levels = CLIENT_COUNTS
    if len(sys.argv) > 1:
        levels = [int(x) for x in sys.argv[1:]]

    run_ts = int(time.time())
    results = []

    for c in levels:
        run_id = f"sqsd-{run_ts}-c{c}"
        r = await run_level(c, run_id)
        results.append(r)

    print(f"\n{'='*60}")
    print(f"  Summary")
    print(f"{'='*60}")
    print(f"  {'Clients':>8} {'Placed':>8} {'Rate':>10} {'Errors':>8}")
    for r in results:
        print(f"  {r['clients']:>8} {r['placed']:>8} {r['rate']:>9.1f}/s {r['errors']:>8}")

def get_esm_uuid():
    """Get event source mapping UUID for the worker Lambda."""
    try:
        return subprocess.check_output([
            "aws", "lambda", "list-event-source-mappings",
            "--function-name", WORKER_LAMBDA,
            "--query", "EventSourceMappings[0].UUID", "--output", "text"
        ], stderr=subprocess.DEVNULL).decode().strip()
    except:
        return None

def purge_queue():
    """Purge remaining SQS messages after benchmark completes."""
    depth = sqs_depth()
    if depth <= 0:
        print(f"\n  Queue empty, no purge needed.")
        return

    print(f"\n  Purging SQS queue ({depth} messages)...", flush=True)
    try:
        esm_uuid = get_esm_uuid()
        if esm_uuid and esm_uuid != "None":
            # Disable ESM to stop re-delivery during purge
            subprocess.check_output([
                "aws", "lambda", "update-event-source-mapping",
                "--uuid", esm_uuid, "--no-enabled"
            ], stderr=subprocess.DEVNULL)
            print("    ESM disabled. Waiting for in-flight messages...", flush=True)
            # Wait for visibility timeout (180s) + buffer
            time.sleep(200)

        subprocess.check_output([
            "aws", "sqs", "purge-queue", "--queue-url", SQS_URL
        ], stderr=subprocess.DEVNULL)
        print("    Purge requested. Waiting for completion...", flush=True)
        time.sleep(65)

        if esm_uuid and esm_uuid != "None":
            subprocess.check_output([
                "aws", "lambda", "update-event-source-mapping",
                "--uuid", esm_uuid, "--enabled",
                "--scaling-config", '{"MaximumConcurrency":50}'
            ], stderr=subprocess.DEVNULL)
            print("    ESM re-enabled.", flush=True)

        depth = sqs_depth()
        print(f"    Final queue depth: {depth}")
    except Exception as e:
        print(f"    Purge error: {e}")

if __name__ == "__main__":
    asyncio.run(main())
    purge_queue()
