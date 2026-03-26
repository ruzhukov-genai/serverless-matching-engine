#!/usr/bin/env python3
"""
AWS Benchmark — SQS-Direct mode.
POST /api/orders → API Gateway → SQS Standard → Worker Lambda (ESM).

Reports three mandatory metrics per level:
  1. orders/s   — client dispatch rate
  2. matches/s  — server-side processing rate
  3. execution time — received_at → persisted_at (true E2E including queue wait)
"""
import asyncio, aiohttp, subprocess, json, time, statistics, sys
from datetime import datetime, timezone

API_URL = "https://kpvhsf0ub8.execute-api.us-east-1.amazonaws.com"
WORKER_LAMBDA = "serverless-matching-engine-worker"
SQS_URL = "https://queue.amazonaws.com/210352747749/serverless-matching-engine-orders"
PAIR = "BTC-USDT"
DURATION = 30          # seconds per dispatch phase
DRAIN_WAIT = 30        # seconds to wait for processing after dispatch
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


def get_esm_uuid():
    try:
        return subprocess.check_output([
            "aws", "lambda", "list-event-source-mappings",
            "--function-name", WORKER_LAMBDA,
            "--query", "EventSourceMappings[0].UUID", "--output", "text"
        ], stderr=subprocess.DEVNULL).decode().strip()
    except:
        return None


def query_processing_stats(run_id, dispatch_elapsed):
    """Query DB for processing stats: matches/s and execution time."""
    rows = lambda_query(f"""
        SELECT
            count(*) as total,
            count(matched_at) as matched,
            count(persisted_at) as persisted,
            count(CASE WHEN status = 'Filled' THEN 1 END) as filled,
            round(percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - received_at) * 1000)) as p50_e2e_ms,
            round(percentile_cont(0.95) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - received_at) * 1000)) as p95_e2e_ms,
            round(percentile_cont(0.99) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - received_at) * 1000)) as p99_e2e_ms,
            round(min(extract(epoch from persisted_at - received_at) * 1000)) as min_e2e_ms,
            round(max(extract(epoch from persisted_at - received_at) * 1000)) as max_e2e_ms,
            round(percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch from matched_at - received_at) * 1000)) as p50_match_ms,
            round(percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - matched_at) * 1000)) as p50_persist_ms
        FROM orders
        WHERE client_order_id LIKE '{run_id}%'
          AND persisted_at IS NOT NULL
    """)
    return rows[0] if rows else {}


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

    # 1. ORDERS/S (dispatch)
    print(f"\n  📤 Dispatch:")
    print(f"    {placed} orders in {elapsed:.1f}s = {rate:.1f} ord/s  (errors: {errors})")
    if latencies:
        latencies.sort()
        p50 = latencies[len(latencies)//2]
        p95 = latencies[int(len(latencies)*0.95)]
        p99 = latencies[int(len(latencies)*0.99)]
        print(f"    Client latency: p50={p50:.0f}ms  p95={p95:.0f}ms  p99={p99:.0f}ms")

    # Wait for processing
    print(f"\n  ⏳ Waiting {DRAIN_WAIT}s for processing...", end="", flush=True)
    for i in range(DRAIN_WAIT // 5):
        time.sleep(5)
        d = sqs_depth()
        print(f" [{d}]", end="", flush=True)
        if d <= 0:
            break
    print()

    # 2 & 3. MATCHES/S + EXECUTION TIME
    stats = query_processing_stats(run_id, elapsed)
    total = stats.get("total", 0)
    matched = stats.get("matched", 0)
    filled = stats.get("filled", 0)
    remaining = sqs_depth()

    # Calculate matches/s: processed orders / total wall time (dispatch + drain wait)
    total_wall = elapsed + DRAIN_WAIT
    matches_per_sec = matched / total_wall if total_wall > 0 else 0

    print(f"  📊 Processing:")
    print(f"    Processed: {total}/{placed} ({100*total/placed:.0f}% of dispatched)")
    print(f"    Matched: {matched}  Filled: {filled}")
    print(f"    Matches/s: {matches_per_sec:.1f}")
    print(f"    Queue remaining: {remaining}")

    print(f"\n  ⏱️  Execution time (received → persisted, includes queue wait):")
    print(f"    p50={stats.get('p50_e2e_ms', '?')}ms  p95={stats.get('p95_e2e_ms', '?')}ms  p99={stats.get('p99_e2e_ms', '?')}ms")
    print(f"    min={stats.get('min_e2e_ms', '?')}ms  max={stats.get('max_e2e_ms', '?')}ms")
    print(f"    (match p50={stats.get('p50_match_ms', '?')}ms  persist p50={stats.get('p50_persist_ms', '?')}ms)")

    return {
        "clients": num_clients,
        "placed": placed,
        "dispatch_rate": rate,
        "errors": errors,
        "processed": total,
        "matched": matched,
        "matches_per_sec": matches_per_sec,
        "p50_e2e_ms": stats.get("p50_e2e_ms"),
        "p99_e2e_ms": stats.get("p99_e2e_ms"),
    }


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
    print(f"  {'c':>4} {'Dispatched':>10} {'ord/s':>8} {'Processed':>10} {'match/s':>8} {'E2E p50':>8} {'E2E p99':>8} {'Err':>5}")
    for r in results:
        p50 = f"{r['p50_e2e_ms']:.0f}ms" if r['p50_e2e_ms'] is not None else "?"
        p99 = f"{r['p99_e2e_ms']:.0f}ms" if r['p99_e2e_ms'] is not None else "?"
        print(f"  {r['clients']:>4} {r['placed']:>10} {r['dispatch_rate']:>7.1f}/s {r['processed']:>10} {r['matches_per_sec']:>7.1f}/s {p50:>8} {p99:>8} {r['errors']:>5}")


def purge_queue():
    """Purge remaining SQS messages after benchmark completes."""
    depth = sqs_depth()
    if depth <= 0:
        print(f"\n  ✅ Queue empty, no purge needed.")
        return

    print(f"\n  🗑️  Purging SQS queue ({depth} messages)...", flush=True)
    try:
        esm_uuid = get_esm_uuid()
        if esm_uuid and esm_uuid != "None":
            subprocess.check_output([
                "aws", "lambda", "update-event-source-mapping",
                "--uuid", esm_uuid, "--no-enabled"
            ], stderr=subprocess.DEVNULL)
            print("    ESM disabled. Waiting for in-flight messages (200s)...", flush=True)
            time.sleep(200)

        subprocess.check_output([
            "aws", "sqs", "purge-queue", "--queue-url", SQS_URL
        ], stderr=subprocess.DEVNULL)
        print("    Purge requested. Waiting 65s...", flush=True)
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
