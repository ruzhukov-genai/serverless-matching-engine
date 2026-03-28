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
GATEWAY_LAMBDA = "serverless-matching-engine-gateway"
PAIR = "BTC-USDT"
DB_DSN = "postgres://sme:sme_prod_9b43c1802d8440e9666be882d925d933@serverless-matching-engine-proxy.proxy-cp3apgpybbhw.us-east-1.rds.amazonaws.com:5432/matching_engine"
DURATION = 30  # seconds per level
CLIENT_COUNTS = [1, 40, 100]
RATE_PER_CLIENT = 0  # orders/s per client (0 = unlimited)
NUM_USERS = 100  # must match reset_all users count

# ── Helpers ───────────────────────────────────────────────────────────────────
def manage_request(command: str, **kwargs) -> dict:
    """Send a manage command to the Gateway's internal /internal/manage endpoint."""
    import urllib.request
    payload = {"command": command, **kwargs}
    data = json.dumps(payload).encode()
    req = urllib.request.Request(
        f"{API_URL}/internal/manage",
        data=data,
        headers={"Content-Type": "application/json"},
        method="POST"
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return json.loads(resp.read())
    except Exception as e:
        print(f"  ⚠️  manage_request failed: {e}")
        return {"error": str(e)}

def pg_reset_all(num_users: int = 100):
    """Reset all benchmark data via Gateway /internal/manage."""
    resp = manage_request("reset_all", num_users=num_users)
    if not resp.get("ok"):
        print(f"  ⚠️  reset_all failed: {resp}")

def lambda_invoke(payload: dict) -> dict:
    """Synchronous Lambda invoke on the Gateway Lambda, returns parsed response."""
    try:
        subprocess.check_output([
            "aws", "lambda", "invoke",
            "--function-name", GATEWAY_LAMBDA,
            "--payload", json.dumps(payload),
            "/tmp/_bench_invoke.json"
        ], stderr=subprocess.DEVNULL)
        with open("/tmp/_bench_invoke.json") as f:
            return json.load(f)
    except Exception as e:
        return {"error": str(e)}

def lambda_query(sql: str) -> list:
    """Run SQL query via Gateway /internal/manage."""
    resp = manage_request("query", sql=sql)
    return resp.get("rows", [])

# ── Order placer ──────────────────────────────────────────────────────────────
async def place_orders(session, num_clients, run_id, deadline):
    """Place orders as fast as possible with num_clients concurrency."""
    placed = 0
    errors = 0
    latencies = []
    users = [f"user-{(i % NUM_USERS) + 1}" for i in range(num_clients)]

    async def client_loop(client_id):
        nonlocal placed, errors
        seq = 0
        interval = 1.0 / RATE_PER_CLIENT if RATE_PER_CLIENT > 0 else 0
        next_send = time.monotonic()
        while time.monotonic() < deadline:
            # Rate limiting: sleep until next allowed send time
            # next_send is set BEFORE the request so interval is wall-clock, not request-completion relative
            if interval > 0:
                now = time.monotonic()
                if now < next_send:
                    await asyncio.sleep(next_send - now)
            next_send_before = time.monotonic()  # capture before request

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
                        if errors <= 3:
                            body_text = await resp.text()
                            print(f"  ⚠️  HTTP {resp.status}: {body_text[:120]}")
            except Exception as e:
                errors += 1
                if errors <= 3:
                    print(f"  ⚠️  Exception: {e}")
            # Schedule next send relative to when this send started (not when it completed)
            if interval > 0:
                next_send = next_send_before + interval
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
    global API_URL, DURATION, CLIENT_COUNTS, DRAIN_PARALLELISM, RATE_PER_CLIENT, NUM_USERS

    import argparse
    parser = argparse.ArgumentParser(description="AWS Benchmark with continuous drain")
    parser.add_argument("--clients", type=str, default=None)
    parser.add_argument("--duration", type=int, default=DURATION)
    parser.add_argument("--run-id", type=str, default=None)
    parser.add_argument("--api", type=str, default=API_URL)
    parser.add_argument("--rate", type=float, default=RATE_PER_CLIENT, help="orders/s per client (0=unlimited)")
    parser.add_argument("--users", type=int, default=NUM_USERS, help="number of users (must match reset_all users count)")
    args = parser.parse_args()

    API_URL = args.api
    DURATION = args.duration
    RATE_PER_CLIENT = args.rate
    NUM_USERS = args.users
    if args.clients:
        CLIENT_COUNTS = [int(x.strip()) for x in args.clients.split(",")]

    run_id = args.run_id or f"awsbench-{datetime.now(timezone.utc).strftime('%Y%m%d-%H%M')}"

    print("╔══════════════════════════════════════════════════════════════╗")
    print("║     AWS Benchmark — Continuous Drain (real-time E2E)       ║")
    print("╠══════════════════════════════════════════════════════════════╣")
    print(f"║  API:      {API_URL:<48}║")
    print(f"║  Run ID:   {run_id:<48}║")
    rate_str = f"{RATE_PER_CLIENT}/s per client" if RATE_PER_CLIENT > 0 else "unlimited"
    print(f"║  Duration: {DURATION}s/level, clients: {CLIENT_COUNTS!s:<28}║")
    print(f"║  Rate:     {rate_str:<48}║")
    print(f"║  Users:    {NUM_USERS:<48}║")
    print("╚══════════════════════════════════════════════════════════════╝")

    # Reset
    print("\n  🔄 Resetting AWS data...")
    pg_reset_all(num_users=NUM_USERS)

    all_results = []

    for num_clients in CLIENT_COUNTS:
        print(f"\n{'━' * 65}")
        print(f"  ▶ c={num_clients} — {DURATION}s")
        print(f"{'━' * 65}")

        # Seed liquidity
        async with aiohttp.ClientSession() as session:
            seeded = await seed_liquidity(session, run_id)
        print(f"  📦 Seeded {seeded} resting orders")

        # Place orders
        deadline = time.monotonic() + DURATION
        async with aiohttp.ClientSession() as session:
            placed, errors, latencies = await place_orders(session, num_clients, run_id, deadline)

                # In inline mode, orders are already processed synchronously — no drain needed
        print(f"  ⏳ Placed {placed} orders ({errors} errors)")

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

# ── Ramp benchmark ────────────────────────────────────────────────────────────
async def run_ramp(api_url: str, run_id: str, num_users: int,
                   step_size: float, step_interval: float, max_rate: float, workers: int):
    """
    Ramp benchmark: start at step_size ord/s, add step_size every step_interval seconds.
    Uses a shared token bucket across `workers` async sender coroutines.
    Prints per-second stats and stops at max_rate or when errors pile up.
    """
    import collections

    print("╔══════════════════════════════════════════════════════════════╗")
    print("║          AWS Benchmark — Ramp Mode                         ║")
    print("╠══════════════════════════════════════════════════════════════╣")
    print(f"║  API:      {api_url:<48}║")
    print(f"║  Run ID:   {run_id:<48}║")
    print(f"║  Ramp:     +{step_size}/s every {step_interval}s → max {max_rate}/s{' ':<22}║")
    print(f"║  Workers:  {workers:<48}║")
    print(f"║  Users:    {num_users:<48}║")
    print("╚══════════════════════════════════════════════════════════════╝")

    print("\n  🔄 Resetting AWS data...")
    pg_reset_all(num_users=NUM_USERS)

    print(f"  📦 Seeding liquidity...")
    async with aiohttp.ClientSession() as session:
        seeded = await seed_liquidity(session, run_id)
    print(f"  📦 Seeded {seeded} resting orders")

    # Token bucket state
    current_rate = step_size
    token_lock = asyncio.Lock()
    tokens = step_size  # start full
    last_refill = time.monotonic()

    placed_total = 0
    errors_total = 0
    stop_flag = asyncio.Event()

    # Per-second window tracking
    window = collections.deque()  # (timestamp, success)
    window_lock = asyncio.Lock()

    async def refill_loop():
        nonlocal current_rate, tokens, last_refill
        start = time.monotonic()
        next_step = start + step_interval
        while not stop_flag.is_set():
            await asyncio.sleep(0.01)
            now = time.monotonic()
            async with token_lock:
                elapsed = now - last_refill
                tokens = min(current_rate, tokens + current_rate * elapsed)
                last_refill = now
            # Step up rate
            if now >= next_step:
                current_rate = min(max_rate, current_rate + step_size)
                next_step = now + step_interval

    async def sender(worker_id: int):
        nonlocal placed_total, errors_total, tokens, last_refill
        seq = 0
        connector = aiohttp.TCPConnector(limit=0)  # no connection pool limit for sync mode
        async with aiohttp.ClientSession(connector=connector,
                                         timeout=aiohttp.ClientTimeout(total=30)) as session:
            while not stop_flag.is_set():
                # Consume a token
                got_token = False
                for _ in range(50):  # spin up to 50ms waiting for token
                    async with token_lock:
                        if tokens >= 1.0:
                            tokens -= 1.0
                            got_token = True
                            break
                    await asyncio.sleep(0.001)
                if not got_token:
                    await asyncio.sleep(0.005)
                    continue

                user = f"user-{(worker_id * 31 + seq) % num_users + 1}"
                side = "Buy" if seq % 2 == 0 else "Sell"
                coid = f"{run_id}-w{worker_id}-{seq}"
                body = {
                    "pair_id": PAIR, "user_id": user, "side": side,
                    "order_type": "Limit", "price": "70700",
                    "quantity": "0.001", "time_in_force": "GTC",
                    "client_order_id": coid
                }
                t0 = time.monotonic()
                try:
                    async with session.post(f"{api_url}/api/orders", json=body,
                                            timeout=aiohttp.ClientTimeout(total=5)) as resp:
                        ok = resp.status in (200, 201, 202)
                        async with window_lock:
                            window.append((t0, ok))
                        if ok:
                            placed_total += 1
                        else:
                            errors_total += 1
                except Exception:
                    errors_total += 1
                seq += 1

    async def reporter():
        """Print one line per second showing current rate and throughput."""
        print(f"\n  {'Time':>5} {'Target':>8} {'Actual':>8} {'Placed':>8} {'Errors':>7}")
        print(f"  {'─'*5} {'─'*8} {'─'*8} {'─'*8} {'─'*7}")
        t_start = time.monotonic()
        while not stop_flag.is_set():
            await asyncio.sleep(1.0)
            now = time.monotonic()
            elapsed = now - t_start
            # Count successes in last 1s window
            cutoff = now - 1.0
            async with window_lock:
                while window and window[0][0] < cutoff - 1.0:
                    window.popleft()
                recent = [ok for ts, ok in window if ts >= cutoff]
            actual = sum(recent)
            async with token_lock:
                rate_now = current_rate
            print(f"  {elapsed:>5.1f}s {rate_now:>7.0f}/s {actual:>7.0f}/s {placed_total:>8} {errors_total:>7}")

    # Launch
    tasks = [asyncio.create_task(refill_loop()),
             asyncio.create_task(reporter())]
    tasks += [asyncio.create_task(sender(i)) for i in range(workers)]

    # Run until max_rate sustained for 5s or manual stop
    total_time = (max_rate / step_size) * step_interval + 10
    await asyncio.sleep(total_time)
    stop_flag.set()
    for t in tasks:
        t.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)

    print(f"\n  ✅ Ramp complete. Placed: {placed_total}, Errors: {errors_total}")
    # In async mode, Worker processes after Gateway returns — wait for drain
    # Poll DB until matched count stabilises or 120s timeout
    print(f"\n  ⏳ Waiting for Worker Lambda to process all orders (max 120s)...")
    prefix = f"{run_id}-"
    last_count = -1
    stable_rounds = 0
    for _ in range(24):  # up to 120s (24 × 5s)
        await asyncio.sleep(5)
        rows = lambda_query(f"SELECT count(*) as n FROM orders WHERE client_order_id LIKE '{prefix}%' AND matched_at IS NOT NULL")
        matched_so_far = rows[0].get("n", 0) if rows else 0
        print(f"    processed so far: {matched_so_far}/{placed_total}")
        if matched_so_far == last_count:
            stable_rounds += 1
            if stable_rounds >= 2:
                break
        else:
            stable_rounds = 0
        last_count = matched_so_far

    # Query lifecycle for the full run
    prefix = f"{run_id}-"
    sql = f"""
    SELECT
        count(*) as total,
        count(matched_at) as matched,
        count(persisted_at) as persisted,
        percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch from matched_at - received_at) * 1000) as match_p50,
        percentile_cont(0.99) WITHIN GROUP (ORDER BY extract(epoch from matched_at - received_at) * 1000) as match_p99,
        percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - received_at) * 1000) as e2e_p50,
        percentile_cont(0.99) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - received_at) * 1000) as e2e_p99,
        percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - matched_at) * 1000) as persist_p50,
        percentile_cont(0.99) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - matched_at) * 1000) as persist_p99
    FROM orders
    WHERE client_order_id LIKE '{prefix}%'
      AND received_at IS NOT NULL AND matched_at IS NOT NULL AND persisted_at IS NOT NULL
    """
    rows = lambda_query(sql)
    lc = rows[0] if rows else {}

    matched = lc.get("matched", 0)
    persisted = lc.get("persisted", 0)
    e2e_p50 = lc.get("e2e_p50") or 0
    e2e_p99 = lc.get("e2e_p99") or 0
    match_p50 = lc.get("match_p50") or 0
    match_p99 = lc.get("match_p99") or 0
    persist_p50 = lc.get("persist_p50") or 0
    persist_p99 = lc.get("persist_p99") or 0

    print(f"\n{'═' * 70}")
    print(f"  RAMP RESULTS — {run_id}")
    print(f"{'═' * 70}")
    print(f"  Dispatched:  {placed_total} orders, {errors_total} errors")
    print(f"  Processed:   {matched} matched, {persisted} persisted")
    print(f"  Match    p50={match_p50:.1f}ms  p99={match_p99:.1f}ms")
    print(f"  Persist  p50={persist_p50:.1f}ms  p99={persist_p99:.1f}ms")
    print(f"  E2E      p50={e2e_p50:.1f}ms  p99={e2e_p99:.1f}ms  (recv→persist)")
    print(f"{'═' * 70}")


async def main():
    import argparse
    parser = argparse.ArgumentParser(description="AWS Benchmark")
    parser.add_argument("--ramp", action="store_true", help="ramp mode: +N/s every T seconds")
    parser.add_argument("--ramp-step", type=float, default=10, help="ord/s added per step (default 10)")
    parser.add_argument("--ramp-interval", type=float, default=1.0, help="seconds per step (default 1)")
    parser.add_argument("--ramp-max", type=float, default=200, help="max ord/s (default 200)")
    parser.add_argument("--ramp-workers", type=int, default=50, help="sender coroutines (default 50)")
    parser.add_argument("--users", type=int, default=NUM_USERS)
    parser.add_argument("--run-id", type=str, default=None)
    # passthrough args for non-ramp mode
    parser.add_argument("--clients", type=str, default=None)
    parser.add_argument("--duration", type=int, default=DURATION)
    parser.add_argument("--api", type=str, default=API_URL)
    parser.add_argument("--rate", type=float, default=RATE_PER_CLIENT)
    args = parser.parse_args()

    run_id = args.run_id or f"awsbench-{datetime.now(timezone.utc).strftime('%Y%m%d-%H%M')}"

    if args.ramp:
        await run_ramp(
            api_url=args.api,
            run_id=run_id,
            num_users=args.users,
            step_size=args.ramp_step,
            step_interval=args.ramp_interval,
            max_rate=args.ramp_max,
            workers=args.ramp_workers,
        )
    else:
        await run()

if __name__ == "__main__":
    asyncio.run(main())