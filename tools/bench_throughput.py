#!/usr/bin/env python3
"""
Throughput & latency benchmark: measures orders/sec, trades/sec, and order
latency (p50, p99) for 1–100 concurrent users. No sleep between orders —
each user fires as fast as possible.
"""

import asyncio
import aiohttp
import time
import json
import subprocess
import statistics
from dataclasses import dataclass, field

BASE_URL = "http://localhost:3001"
PAIR = "BTC-USDT"
TEST_DURATION = 15  # seconds per concurrency level
CLIENT_COUNTS = [1, 2, 5, 10, 25, 50, 100]


def db_counts():
    """Get current order/trade counts from PG."""
    try:
        out = subprocess.check_output([
            "docker", "exec", "sme-postgres", "psql", "-U", "sme",
            "-d", "matching_engine", "-t", "-c",
            "SELECT json_build_object('orders', (SELECT count(*) FROM orders), "
            "'trades', (SELECT count(*) FROM trades));"
        ], text=True).strip()
        return json.loads(out)
    except:
        return {"orders": 0, "trades": 0}


async def place_orders(session, client_id, deadline, latencies, errors):
    """One client placing orders as fast as possible, alternating buy/sell."""
    seq = 0
    user = f"user-{(client_id % 10) + 1}"
    while time.monotonic() < deadline:
        # Alternate sides + prices to generate matches
        pattern = seq % 4
        if pattern == 0:   side, price = "Buy",  "70800.00"
        elif pattern == 1: side, price = "Sell", "70800.00"
        elif pattern == 2: side, price = "Buy",  "70600.00"
        else:              side, price = "Sell", "70600.00"

        body = {
            "user_id": user, "pair_id": PAIR, "side": side,
            "order_type": "Limit", "tif": "GTC",
            "price": price, "quantity": "0.00100",
        }
        t0 = time.monotonic()
        try:
            async with session.post(f"{BASE_URL}/api/orders", json=body) as resp:
                await resp.read()
                ms = (time.monotonic() - t0) * 1000
                if resp.status in (200, 201, 202):
                    latencies.append(ms)
                else:
                    errors.append(resp.status)
        except Exception as e:
            errors.append(str(e))
        seq += 1


async def run_level(num_clients):
    """Run one concurrency level and return stats."""
    db_before = db_counts()
    latencies = []
    errors = []

    deadline = time.monotonic() + TEST_DURATION
    connector = aiohttp.TCPConnector(limit=num_clients * 2, force_close=False)
    async with aiohttp.ClientSession(connector=connector) as session:
        tasks = [
            asyncio.create_task(place_orders(session, i, deadline, latencies, errors))
            for i in range(num_clients)
        ]
        await asyncio.gather(*tasks)

    db_after = db_counts()
    new_orders = db_after["orders"] - db_before["orders"]
    new_trades = db_after["trades"] - db_before["trades"]

    if latencies:
        s = sorted(latencies)
        n = len(s)
        return {
            "clients": num_clients,
            "total_submitted": n,
            "errors": len(errors),
            "orders_sec": round(n / TEST_DURATION, 1),
            "new_orders_db": new_orders,
            "new_trades_db": new_trades,
            "trades_sec": round(new_trades / TEST_DURATION, 1),
            "avg_ms": round(statistics.mean(s), 2),
            "p50_ms": round(s[n // 2], 2),
            "p99_ms": round(s[int(n * 0.99)], 2),
            "max_ms": round(s[-1], 2),
        }
    else:
        return {
            "clients": num_clients,
            "total_submitted": 0,
            "errors": len(errors),
            "orders_sec": 0, "trades_sec": 0,
            "new_orders_db": new_orders, "new_trades_db": new_trades,
            "avg_ms": 0, "p50_ms": 0, "p99_ms": 0, "max_ms": 0,
        }


async def main():
    print(f"SME Throughput Benchmark — {TEST_DURATION}s per level, levels: {CLIENT_COUNTS}")
    print(f"Target: {BASE_URL}, pair: {PAIR}")
    print()

    results = []
    for nc in CLIENT_COUNTS:
        print(f"  Running {nc} client(s)...", end="", flush=True)
        r = await run_level(nc)
        results.append(r)
        print(f" {r['orders_sec']} ord/s, {r['trades_sec']} trd/s, "
              f"p50={r['p50_ms']}ms p99={r['p99_ms']}ms "
              f"({r['errors']} errors)")
        # Brief pause between levels
        await asyncio.sleep(2)

    # Summary
    print()
    print(f"{'Clients':>8}  {'Ord/s':>8}  {'Trd/s':>8}  {'p50 ms':>9}  {'p99 ms':>9}  {'max ms':>9}  {'Submitted':>10}  {'Errors':>7}")
    print("─" * 80)
    for r in results:
        print(f"{r['clients']:>8}  {r['orders_sec']:>8.1f}  {r['trades_sec']:>8.1f}  "
              f"{r['p50_ms']:>9.2f}  {r['p99_ms']:>9.2f}  {r['max_ms']:>9.2f}  "
              f"{r['total_submitted']:>10}  {r['errors']:>7}")

    print(f"\nDone.")


if __name__ == "__main__":
    asyncio.run(main())
