#!/usr/bin/env python3
"""
AWS Benchmark — Direct Invoke mode (synchronous matching).
Orders are matched immediately by the worker Lambda via gateway direct invoke.
No queue drain needed — DB lifecycle timestamps are real E2E.
"""

import asyncio
import aiohttp
import subprocess
import json
import time
import statistics
from datetime import datetime, timezone

API_URL = "https://kpvhsf0ub8.execute-api.us-east-1.amazonaws.com"
WORKER_LAMBDA = "serverless-matching-engine-worker"
PAIR = "BTC-USDT"
DURATION = 30
CLIENT_COUNTS = [1, 40, 100]


def lambda_invoke(payload: dict) -> dict:
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
    resp = lambda_invoke({"manage": {"command": "query", "sql": sql}})
    return resp.get("rows", [])


async def place_orders(session, num_clients, run_id, deadline):
    placed = 0
    errors = 0
    latencies = []

    async def client_loop(client_id):
        nonlocal placed, errors
        seq = 0
        while time.monotonic() < deadline:
            user = f"user-{(client_id % 10) + 1}"
            side = "Buy" if seq % 2 == 0 else "Sell"
            price = "70700"
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


def query_lifecycle(run_id, num_clients):
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


async def run():
    global API_URL, DURATION, CLIENT_COUNTS

    import argparse
    parser = argparse.ArgumentParser(description="AWS Benchmark — Direct Invoke")
    parser.add_argument("--clients", type=str, default=None)
    parser.add_argument("--duration", type=int, default=DURATION)
    parser.add_argument("--run-id", type=str, default=None)
    parser.add_argument("--api", type=str, default=API_URL)
    args = parser.parse_args()

    API_URL = args.api
    DURATION = args.duration
    if args.clients:
        CLIENT_COUNTS = [int(x.strip()) for x in args.clients.split(",")]

    run_id = args.run_id or f"awsd-{datetime.now(timezone.utc).strftime('%Y%m%d-%H%M')}"

    print("╔══════════════════════════════════════════════════════════════╗")
    print("║  AWS Benchmark — Direct Invoke (synchronous matching)      ║")
    print("╠══════════════════════════════════════════════════════════════╣")
    print(f"║  API:      {API_URL:<48}║")
    print(f"║  Run ID:   {run_id:<48}║")
    print(f"║  Duration: {DURATION}s/level, clients: {CLIENT_COUNTS!s:<28}║")
    print("╚══════════════════════════════════════════════════════════════╝")

    # Reset
    print("\n  🔄 Resetting AWS data...")
    lambda_invoke({"manage": {"command": "reset_all"}})

    all_results = []

    for num_clients in CLIENT_COUNTS:
        print(f"\n{'━' * 65}")
        print(f"  ▶ c={num_clients} — {DURATION}s")
        print(f"{'━' * 65}")

        # Seed liquidity
        async with aiohttp.ClientSession() as session:
            seeded = await seed_liquidity(session, run_id)
        print(f"  📦 Seeded {seeded} resting orders")
        await asyncio.sleep(2)  # let seeds process

        # Place orders
        deadline = time.monotonic() + DURATION
        async with aiohttp.ClientSession() as session:
            placed, errors, latencies = await place_orders(session, num_clients, run_id, deadline)

        # Give a moment for any in-flight orders to complete
        await asyncio.sleep(2)

        # Client metrics
        latencies.sort()
        n = len(latencies)
        client_p50 = latencies[n // 2] if n else 0
        client_p95 = latencies[int(n * 0.95)] if n else 0
        client_p99 = latencies[int(n * 0.99)] if n else 0
        rate = placed / DURATION if placed else 0

        # DB lifecycle
        lc = query_lifecycle(run_id, num_clients)
        trades = query_trades(run_id, num_clients)

        matched = lc.get("matched", 0)
        persisted = lc.get("persisted", 0)

        def g(key):
            v = lc.get(key, 0)
            return v if v else 0

        print(f"\n  📊 Client: {placed} placed, {errors} errors, {rate:.1f}/s")
        print(f"    Gateway latency: p50={client_p50:.0f}ms  p95={client_p95:.0f}ms  p99={client_p99:.0f}ms")
        print(f"\n  🗄️  DB: {matched}/{placed} matched, {persisted} persisted, {trades} trades")
        print(f"    Match    (recv→match):   p50={g('match_p50'):>8.1f}ms  p95={g('match_p95'):>8.1f}ms  p99={g('match_p99'):>8.1f}ms")
        print(f"    Persist  (match→DB):     p50={g('persist_p50'):>8.1f}ms  p95={g('persist_p95'):>8.1f}ms  p99={g('persist_p99'):>8.1f}ms")
        print(f"    E2E      (recv→persist): p50={g('e2e_p50'):>8.1f}ms  p95={g('e2e_p95'):>8.1f}ms  p99={g('e2e_p99'):>8.1f}ms")

        all_results.append({
            "clients": num_clients, "placed": placed, "errors": errors, "rate": round(rate, 1),
            "matched": matched, "persisted": persisted, "trades": trades,
            "client_p50": round(client_p50, 1), "client_p95": round(client_p95, 1), "client_p99": round(client_p99, 1),
            "e2e_p50": round(g('e2e_p50'), 1), "e2e_p95": round(g('e2e_p95'), 1), "e2e_p99": round(g('e2e_p99'), 1),
            "match_p50": round(g('match_p50'), 1), "match_p99": round(g('match_p99'), 1),
            "persist_p50": round(g('persist_p50'), 1), "persist_p99": round(g('persist_p99'), 1),
        })

    # Summary
    print(f"\n{'═' * 100}")
    print("  SUMMARY — Order Placed → Match Persisted in DB")
    print(f"{'═' * 100}")
    print(f"{'c':>5} {'Ord/s':>7} {'Placed':>7} {'Matched':>8} {'Trades':>7} {'GW p50':>8} {'GW p99':>8} {'E2E p50':>9} {'E2E p99':>9} {'Match p50':>10} {'Persist p50':>12}")
    print(f"{'─' * 100}")
    for r in all_results:
        print(f"{r['clients']:>5} {r['rate']:>7.1f} {r['placed']:>7} {r['matched']:>8} {r['trades']:>7} "
              f"{r['client_p50']:>7.0f}ms {r['client_p99']:>7.0f}ms "
              f"{r['e2e_p50']:>8.1f}ms {r['e2e_p99']:>8.1f}ms "
              f"{r['match_p50']:>9.1f}ms {r['persist_p50']:>11.1f}ms")

    print(f"\n✅ Done. Run ID: {run_id}")


if __name__ == "__main__":
    asyncio.run(run())
