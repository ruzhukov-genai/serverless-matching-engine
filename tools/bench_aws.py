#!/usr/bin/env python3
"""
AWS-targeted throughput benchmark with Lambda warmup phase.
Measures client-side latency at a fixed concurrency level.

Usage:
  python3 tools/bench_aws.py -c 40 -d 60
  python3 tools/bench_aws.py -c 100 -d 30 --no-warmup
  python3 tools/bench_aws.py --api https://your-api.execute-api.us-east-1.amazonaws.com -c 20
"""

import asyncio
import aiohttp
import time
import statistics
import argparse

PAIR = "BTC-USDT"


async def place_orders(session, base_url, client_id, deadline, latencies, errors):
    seq = 0
    user = f"user-{(client_id % 10) + 1}"
    while time.monotonic() < deadline:
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
            async with session.post(f"{base_url}/api/orders", json=body) as resp:
                await resp.read()
                ms = (time.monotonic() - t0) * 1000
                if resp.status in (200, 201, 202):
                    latencies.append(ms)
                else:
                    errors.append(f"HTTP {resp.status}")
        except Exception as e:
            errors.append(str(e))
        seq += 1


async def warmup(base_url, target_concurrency, warmup_duration=15):
    """
    Warm up Lambda instances by ramping up concurrency gradually.
    Sends orders at increasing concurrency to pre-initialize Lambda instances,
    PG connection pools, and Dragonfly connections before the real benchmark.
    """
    # Ramp: 5 → target_concurrency/2 → target_concurrency over warmup_duration
    ramp_levels = [
        (min(5, target_concurrency), warmup_duration // 3),
        (min(target_concurrency // 2, target_concurrency), warmup_duration // 3),
        (target_concurrency, warmup_duration // 3),
    ]

    total_warmed = 0
    total_errors = 0

    for nc, dur in ramp_levels:
        if dur < 1:
            dur = 1
        latencies = []
        errors = []
        connector = aiohttp.TCPConnector(limit=nc * 2, force_close=False)
        deadline = time.monotonic() + dur

        async with aiohttp.ClientSession(connector=connector) as session:
            tasks = [
                asyncio.create_task(
                    place_orders(session, base_url, i, deadline, latencies, errors)
                )
                for i in range(nc)
            ]
            await asyncio.gather(*tasks)

        total_warmed += len(latencies)
        total_errors += len(errors)
        print(f"  Warmup c={nc:>3}: {len(latencies)} orders, {len(errors)} errors", flush=True)

    # Brief pause to let async workers finish processing
    await asyncio.sleep(2)
    return total_warmed, total_errors


async def main():
    parser = argparse.ArgumentParser(description="AWS SME Benchmark")
    parser.add_argument("--api", default="https://kpvhsf0ub8.execute-api.us-east-1.amazonaws.com",
                        help="API Gateway base URL")
    parser.add_argument("-c", "--concurrency", type=int, default=40,
                        help="Number of concurrent clients")
    parser.add_argument("-d", "--duration", type=int, default=60,
                        help="Benchmark duration in seconds")
    parser.add_argument("--warmup", type=int, default=15,
                        help="Warmup duration in seconds (0 to skip)")
    parser.add_argument("--no-warmup", action="store_true",
                        help="Skip warmup phase entirely")
    args = parser.parse_args()

    base = args.api.rstrip("/")
    nc = args.concurrency
    dur = args.duration

    print(f"╔══════════════════════════════════════════════╗")
    print(f"║  AWS Benchmark: c={nc}, duration={dur}s")
    print(f"║  Target: {base}")
    print(f"║  Start:  {time.strftime('%Y-%m-%d %H:%M:%S', time.gmtime())} UTC")
    print(f"╚══════════════════════════════════════════════╝")
    print()

    # ── Warmup phase ──
    if not args.no_warmup and args.warmup > 0:
        print(f"🔥 Warmup phase ({args.warmup}s, ramping to c={nc})...")
        warmed, warm_errs = await warmup(base, nc, args.warmup)
        print(f"  → {warmed} orders sent, {warm_errs} errors")
        print()

    # ── Main benchmark ──
    print(f"📊 Benchmark phase ({dur}s, c={nc})...")
    latencies = []
    errors = []

    connector = aiohttp.TCPConnector(limit=nc * 2, force_close=False)
    deadline = time.monotonic() + dur

    async with aiohttp.ClientSession(connector=connector) as session:
        tasks = [
            asyncio.create_task(place_orders(session, base, i, deadline, latencies, errors))
            for i in range(nc)
        ]
        await asyncio.gather(*tasks)

    end_time = time.strftime('%Y-%m-%d %H:%M:%S', time.gmtime())

    if latencies:
        s = sorted(latencies)
        n = len(s)
        total = n + len(errors)
        rate = n / dur

        print()
        print(f"End: {end_time} UTC")
        print()
        print(f"Submitted:  {total}")
        print(f"Successful: {n}")
        print(f"Errors:     {len(errors)}")
        print(f"Rate:       {rate:.1f} orders/sec")
        print()
        print(f"Client latency (ms):")
        print(f"  avg:  {statistics.mean(s):.1f}")
        print(f"  p50:  {s[n//2]:.1f}")
        print(f"  p95:  {s[int(n*0.95)]:.1f}")
        print(f"  p99:  {s[int(n*0.99)]:.1f}")
        print(f"  max:  {s[-1]:.1f}")

        if errors:
            from collections import Counter
            print(f"\nError breakdown:")
            for err, cnt in Counter(errors).most_common(10):
                print(f"  {err}: {cnt}")
    else:
        print(f"No successful requests. Errors: {len(errors)}")
        if errors:
            for e in errors[:10]:
                print(f"  {e}")


if __name__ == "__main__":
    asyncio.run(main())
