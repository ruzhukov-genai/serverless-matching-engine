#!/usr/bin/env python3
"""
AWS Benchmark — proper E2E with continuous drain.

Fires orders at the API Gateway while continuously draining the queue
via parallel Lambda invocations, so orders are processed in near-real-time.
Then queries DB lifecycle timestamps for accurate metrics.
"""

import asyncio
import aiohttp
import subprocess
import json
import time
import statistics
from datetime import datetime, timezone
from concurrent.futures import ThreadPoolExecutor

# ── Config ────────────────────────────────────────────────────────────────────
API_URL = "https://kpvhsf0ub8.execute-api.us-east-1.amazonaws.com"
WORKER_LAMBDA = "serverless-matching-engine-worker"
PAIR = "BTC-USDT"
DURATION = 30  # seconds per level
DRAIN_PARALLELISM = 5  # concurrent drain Lambdas
CLIENT_COUNTS = [1, 40, 100]

# ── Helpers ───────────────────────────────────────────────────────────────────
def lambda_invoke(payload: dict) -> dict:
    """Synchronous Lambda invoke, returns parsed response."""
    try:
        subprocess.check_output([
            "aws", "lambda", "invoke",
            "--function-name", WORKER_LAMBDA,
            "--payload", json.dumps(payload),
            "/tmp/_bench_invoke.json"
        ], stderr=subprocess.DEVNULL)
        with open("/tmp/_bench_invoke.json") as f:
            return json.load(f)
    except Exception as e:
        return {"error": str(e)}

def lambda_query(sql: str) -> list:
    """Run SQL via Lambda manage:query, return rows."""
    resp = lambda_invoke({"manage": {"command": "query", "sql": sql}})
    return resp.get("rows", [])

def drain_once(idx: int) -> int:
    """Single drain invocation, returns orders processed."""
    try:
        subprocess.check_output([
            "aws", "lambda", "invoke",
            "--function-name", WORKER_LAMBDA,
            "--payload", '{"drain_queue": true}',
            f"/tmp/_drain_{idx}.json"
        ], stderr=subprocess.DEVNULL)
        with open(f"/tmp/_drain_{idx}.json") as f:
            resp = json.load(f)
        return resp.get("orders_processed", 0)
    except:
        return 0

# ── Background drainer ────────────────────────────────────────────────────────
class ContinuousDrainer:
    def __init__(self, parallelism=5):
        self.parallelism = parallelism
        self.running = False
        self.total_drained = 0
        self.drain_rounds = 0
        self.executor = ThreadPoolExecutor(max_workers=parallelism + 2)

    def start(self):
        self.running = True
        self.total_drained = 0
        self.drain_rounds = 0
        self._thread_futures = []
        for i in range(self.parallelism):
            fut = self.executor.submit(self._drain_loop, i)
            self._thread_futures.append(fut)

    def stop(self):
        self.running = False
        for fut in self._thread_futures:
            fut.result(timeout=180)  # wait for in-flight drains to finish

    def _drain_loop(self, idx):
        while self.running:
            count = drain_once(idx)
            self.total_drained += count
            self.drain_rounds += 1
            if count == 0:
                time.sleep(0.5)  # brief pause if queue was empty

# ── Order placer ──────────────────────────────────────────────────────────────
async def place_orders(session, num_clients, run_id, deadline):
    """Place orders as fast as possible with num_clients concurrency."""
    placed = 0
    errors = 0
    latencies = []
    users = [f"user-{(i % 10) + 1}" for i in range(num_clients)]

    async def client_loop(client_id):
        nonlocal placed, errors
        seq = 0
        while time.monotonic() < deadline:
            user = users[client_id % len(users)]
            side = "Buy" if seq % 2 == 0 else "Sell"
            # Price that will cross with seeded liquidity
            price = "70700" if side == "Buy" else "70700"
            coid = f"{run_id}-n{num_clients}-c{client_id}-{seq}"
            body = {
                "pair_id": PAIR, "user_id": user, "side": side,
                "order_type": "Limit", "price": price,
                "quantity": "0.001", "time_in_force": "GTC",
                "client_order_id": coid
            }
            t0 = time.monotonic()
            try:
                async with session.post(f"{API_URL}/api/orders", json=body) as resp:
                    elapsed = (time.monotonic() - t0) * 1000
                    if resp.status in (200, 201, 202):
                        placed += 1
                        latencies.append(elapsed)
                    else:
                        errors += 1
            except:
                errors += 1
            seq += 1

    tasks = [asyncio.create_task(client_loop(i)) for i in range(num_clients)]
    await asyncio.gather(*tasks, return_exceptions=True)
    return placed, errors, latencies

async def seed_liquidity(session, run_id):
    """Seed resting orders that won't immediately cross."""
    placed = 0
    for level in range(10):
        for side_idx in range(2):
            side = "Buy" if side_idx == 0 else "Sell"
            base = 69600 + level * 100 if side == "Buy" else 70900 + level * 100
            for j in range(5):
                user = f"user-{(j % 10) + 1}"
                body = {
                    "pair_id": PAIR, "user_id": user, "side": side,
                    "order_type": "Limit", "price": str(base),
                    "quantity": "0.001", "time_in_force": "GTC",
                    "client_order_id": f"{run_id}-seed-{level}-{side_idx}-{j}"
                }
                try:
                    async with session.post(f"{API_URL}/api/orders", json=body) as resp:
                        if resp.status in (200, 201, 202):
                            placed += 1
                except:
                    pass
    return placed

# ── Lifecycle query ───────────────────────────────────────────────────────────
def query_lifecycle(run_id, num_clients):
    """Query DB lifecycle metrics for a specific benchmark level."""
    prefix = f"{run_id}-n{num_clients}-"
    sql = f"""
    SELECT 
        count(*) as total,
        count(matched_at) as matched,
        count(persisted_at) as persisted,
        percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch from matched_at - received_at) * 1000) as match_p50,
        percentile_cont(0.95) WITHIN GROUP (ORDER BY extract(epoch from matched_at - received_at) * 1000) as match_p95,
        percentile_cont(0.99) WITHIN GROUP (ORDER BY extract(epoch from matched_at - received_at) * 1000) as match_p99,
        percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - matched_at) * 1000) as persist_p50,
        percentile_cont(0.95) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - matched_at) * 1000) as persist_p95,
        percentile_cont(0.99) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - matched_at) * 1000) as persist_p99,
        percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - received_at) * 1000) as e2e_p50,
        percentile_cont(0.95) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - received_at) * 1000) as e2e_p95,
        percentile_cont(0.99) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - received_at) * 1000) as e2e_p99
    FROM orders 
    WHERE client_order_id LIKE '{prefix}%' 
      AND received_at IS NOT NULL 
      AND matched_at IS NOT NULL 
      AND persisted_at IS NOT NULL
    """
    rows = lambda_query(sql)
    return rows[0] if rows else {}

def query_trades(run_id, num_clients):
    prefix = f"{run_id}-n{num_clients}-"
    sql = f"SELECT count(DISTINCT t.id) as trades FROM trades t JOIN orders o ON (t.buy_order_id = o.id OR t.sell_order_id = o.id) WHERE o.client_order_id LIKE '{prefix}%'"
    rows = lambda_query(sql)
    return rows[0].get("trades", 0) if rows else 0

def query_unprocessed(run_id, num_clients):
    prefix = f"{run_id}-n{num_clients}-"
    sql = f"SELECT count(*) as n FROM orders WHERE client_order_id LIKE '{prefix}%' AND matched_at IS NULL"
    rows = lambda_query(sql)
    return rows[0].get("n", 0) if rows else 0

# ── Main ──────────────────────────────────────────────────────────────────────
async def run():
    global API_URL, DURATION, CLIENT_COUNTS, DRAIN_PARALLELISM

    import argparse
    parser = argparse.ArgumentParser(description="AWS Benchmark with continuous drain")
    parser.add_argument("--clients", type=str, default=None)
    parser.add_argument("--duration", type=int, default=DURATION)
    parser.add_argument("--run-id", type=str, default=None)
    parser.add_argument("--drain-parallel", type=int, default=DRAIN_PARALLELISM)
    parser.add_argument("--api", type=str, default=API_URL)
    args = parser.parse_args()

    API_URL = args.api
    DURATION = args.duration
    DRAIN_PARALLELISM = args.drain_parallel
    if args.clients:
        CLIENT_COUNTS = [int(x.strip()) for x in args.clients.split(",")]

    run_id = args.run_id or f"awsbench-{datetime.now(timezone.utc).strftime('%Y%m%d-%H%M')}"

    print("╔══════════════════════════════════════════════════════════════╗")
    print("║     AWS Benchmark — Continuous Drain (real-time E2E)       ║")
    print("╠══════════════════════════════════════════════════════════════╣")
    print(f"║  API:      {API_URL:<48}║")
    print(f"║  Run ID:   {run_id:<48}║")
    print(f"║  Duration: {DURATION}s/level, clients: {CLIENT_COUNTS!s:<28}║")
    print(f"║  Drainers: {DRAIN_PARALLELISM} parallel Lambda invocations{' ':>21}║")
    print("╚══════════════════════════════════════════════════════════════╝")

    # Reset
    print("\n  🔄 Resetting AWS data...")
    lambda_invoke({"manage": {"command": "reset_all"}})
    # Drain any leftover queue
    for _ in range(3):
        if drain_once(0) == 0:
            break

    all_results = []

    for num_clients in CLIENT_COUNTS:
        print(f"\n{'━' * 65}")
        print(f"  ▶ c={num_clients} — {DURATION}s")
        print(f"{'━' * 65}")

        # Seed liquidity
        async with aiohttp.ClientSession() as session:
            seeded = await seed_liquidity(session, run_id)
        print(f"  📦 Seeded {seeded} resting orders")

        # Start continuous drainer BEFORE placing orders
        drainer = ContinuousDrainer(parallelism=DRAIN_PARALLELISM)
        drainer.start()
        print(f"  🔄 Drainer started ({DRAIN_PARALLELISM} parallel workers)")

        # Let drainer process seeds first
        await asyncio.sleep(3)

        # Place orders
        deadline = time.monotonic() + DURATION
        async with aiohttp.ClientSession() as session:
            placed, errors, latencies = await place_orders(session, num_clients, run_id, deadline)

        # Let drainer catch up (max 60s)
        print(f"  ⏳ Placed {placed} orders ({errors} errors), waiting for drain...")
        for wait_round in range(12):
            await asyncio.sleep(5)
            unprocessed = query_unprocessed(run_id, num_clients)
            if unprocessed == 0:
                break
            print(f"    {unprocessed} orders still in queue...")
        
        drainer.stop()
        print(f"  ✅ Drainer stopped: {drainer.total_drained} orders processed in {drainer.drain_rounds} rounds")

        # Query lifecycle
        lc = query_lifecycle(run_id, num_clients)
        trades = query_trades(run_id, num_clients)

        # Client metrics
        latencies.sort()
        n = len(latencies)
        client_p50 = latencies[n // 2] if n else 0
        client_p95 = latencies[int(n * 0.95)] if n else 0
        client_p99 = latencies[int(n * 0.99)] if n else 0
        rate = placed / DURATION if placed else 0

        print(f"\n  📊 Client: {placed} orders, {rate:.1f}/s, p50={client_p50:.1f}ms p95={client_p95:.1f}ms p99={client_p99:.1f}ms")
        
        matched = lc.get("matched", 0)
        persisted = lc.get("persisted", 0)
        e2e_p50 = lc.get("e2e_p50", 0) or 0
        e2e_p95 = lc.get("e2e_p95", 0) or 0
        e2e_p99 = lc.get("e2e_p99", 0) or 0
        match_p50 = lc.get("match_p50", 0) or 0
        match_p95 = lc.get("match_p95", 0) or 0
        match_p99 = lc.get("match_p99", 0) or 0
        persist_p50 = lc.get("persist_p50", 0) or 0
        persist_p95 = lc.get("persist_p95", 0) or 0
        persist_p99 = lc.get("persist_p99", 0) or 0

        print(f"  🗄️  DB: {matched} matched, {persisted} persisted, {trades} trades")
        print(f"    E2E      (recv→persist): p50={e2e_p50:>9.1f}ms  p95={e2e_p95:>9.1f}ms  p99={e2e_p99:>9.1f}ms")
        print(f"    Match    (recv→match):   p50={match_p50:>9.1f}ms  p95={match_p95:>9.1f}ms  p99={match_p99:>9.1f}ms")
        print(f"    Persist  (match→DB):     p50={persist_p50:>9.1f}ms  p95={persist_p95:>9.1f}ms  p99={persist_p99:>9.1f}ms")

        all_results.append({
            "clients": num_clients, "placed": placed, "errors": errors, "rate": round(rate, 1),
            "matched": matched, "persisted": persisted, "trades": trades,
            "client_p50": round(client_p50, 1), "client_p99": round(client_p99, 1),
            "e2e_p50": round(e2e_p50, 1), "e2e_p99": round(e2e_p99, 1),
            "match_p50": round(match_p50, 1), "match_p99": round(match_p99, 1),
            "persist_p50": round(persist_p50, 1), "persist_p99": round(persist_p99, 1),
        })

    # ── Summary ───────────────────────────────────────────────────────────
    print(f"\n{'═' * 95}")
    print("  SUMMARY — AWS E2E (order placed → match persisted)")
    print(f"{'═' * 95}")
    print(f"{'c':>5} {'Ord/s':>7} {'Placed':>7} {'Matched':>8} {'Trades':>7} {'Client p50':>11} {'E2E p50':>10} {'E2E p99':>10} {'Match p50':>10} {'Persist p50':>12}")
    print(f"{'─' * 95}")
    for r in all_results:
        print(f"{r['clients']:>5} {r['rate']:>7.1f} {r['placed']:>7} {r['matched']:>8} {r['trades']:>7} "
              f"{r['client_p50']:>10.1f}ms {r['e2e_p50']:>9.1f}ms {r['e2e_p99']:>9.1f}ms "
              f"{r['match_p50']:>9.1f}ms {r['persist_p50']:>11.1f}ms")

    print(f"\n✅ Done. Run ID: {run_id}")

if __name__ == "__main__":
    asyncio.run(run())