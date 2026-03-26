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
SQS_URL = "https://queue.amazonaws.com/210352747749/serverless-matching-engine-orders.fifo"
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

    # Truncate orders for clean measurement
    lambda_invoke({"manage": {"command": "truncate_orders"}})
    time.sleep(1)

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

    # Wait for SQS to drain
    print(f"\n  Waiting for SQS drain...", end="", flush=True)
    for _ in range(120):  # up to 2 min
        depth = sqs_depth()
        if depth <= 0:
            break
        print(f" {depth}", end="", flush=True)
        time.sleep(2)
    print(f" done")

    # Wait a bit more for final Lambda processing
    time.sleep(5)

    # Query DB lifecycle timestamps
    print(f"\n  DB Lifecycle (server-side):")
    rows = lambda_query(f"""
        SELECT
            count(*) as total,
            count(matched_at) as matched,
            count(persisted_at) as persisted,
            avg(extract(epoch from matched_at - received_at) * 1000)::int as avg_match_ms,
            percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch from matched_at - received_at) * 1000)::int as p50_match_ms,
            percentile_cont(0.95) WITHIN GROUP (ORDER BY extract(epoch from matched_at - received_at) * 1000)::int as p95_match_ms,
            percentile_cont(0.99) WITHIN GROUP (ORDER BY extract(epoch from matched_at - received_at) * 1000)::int as p99_match_ms,
            avg(extract(epoch from persisted_at - matched_at) * 1000)::int as avg_persist_ms,
            percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - matched_at) * 1000)::int as p50_persist_ms,
            percentile_cont(0.99) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - matched_at) * 1000)::int as p99_persist_ms,
            avg(extract(epoch from persisted_at - received_at) * 1000)::int as avg_e2e_ms,
            percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - received_at) * 1000)::int as p50_e2e_ms,
            percentile_cont(0.99) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - received_at) * 1000)::int as p99_e2e_ms
        FROM orders WHERE client_order_id LIKE '{run_id}%' AND received_at IS NOT NULL
    """)
    if rows:
        r = rows[0]
        print(f"    Orders: {r['total']} total, {r['matched']} matched, {r['persisted']} persisted")
        print(f"    Match:   avg={r['avg_match_ms']}ms  p50={r['p50_match_ms']}ms  p95={r['p95_match_ms']}ms  p99={r['p99_match_ms']}ms")
        print(f"    Persist: avg={r['avg_persist_ms']}ms  p50={r['p50_persist_ms']}ms  p99={r['p99_persist_ms']}ms")
        print(f"    E2E:     avg={r['avg_e2e_ms']}ms  p50={r['p50_e2e_ms']}ms  p99={r['p99_e2e_ms']}ms")
    else:
        print(f"    (no orders found in DB)")

    # Count rejected orders (in SQS DLQ or just not in DB)
    reject_rows = lambda_query(f"SELECT count(*) as cnt FROM orders WHERE client_order_id LIKE '{run_id}%'")
    db_count = reject_rows[0]['cnt'] if reject_rows else 0
    if placed > 0:
        print(f"    Acceptance: {db_count}/{placed} ({100*db_count/placed:.1f}%)")

    return {"clients": num_clients, "placed": placed, "rate": rate, "errors": errors, "db_count": db_count}

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
    print(f"  {'Clients':>8} {'Placed':>8} {'Rate':>10} {'Errors':>8} {'DB':>8} {'Accept%':>8}")
    for r in results:
        accept = f"{100*r['db_count']/r['placed']:.0f}%" if r['placed'] > 0 else "N/A"
        print(f"  {r['clients']:>8} {r['placed']:>8} {r['rate']:>9.1f}/s {r['errors']:>8} {r['db_count']:>8} {accept:>8}")

if __name__ == "__main__":
    asyncio.run(main())
