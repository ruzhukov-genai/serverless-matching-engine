#!/usr/bin/env python3
"""
UI Client Load Test — simulates multiple browser clients hitting the SME server.

Each simulated client:
  1. Fetches static assets (HTML, CSS, JS) — like first page load
  2. Calls REST APIs (pairs, orderbook, trades, ticker, portfolio)
  3. Opens 2 WebSocket connections (orderbook + trades feeds)
  4. Periodically places orders via POST /api/orders

Measures:
  - Static asset latency
  - REST API latency per endpoint
  - WebSocket connection time + message throughput
  - Order placement latency under concurrent WS load
  - Server resource usage (CPU, memory, connections)
"""

import asyncio
import aiohttp
import time
import json
import sys
import statistics
import subprocess
from dataclasses import dataclass, field
from typing import Dict, List

BASE_URL = "http://localhost:3001"
WS_BASE = "ws://localhost:3001"
PAIR = "BTC-USDT"
TEST_DURATION = 15  # seconds per phase
CLIENT_COUNTS = [1, 5, 10, 25, 50, 100]

@dataclass
class LatencyBucket:
    name: str
    samples: List[float] = field(default_factory=list)
    errors: int = 0

    def record(self, ms: float):
        self.samples.append(ms)

    def record_error(self):
        self.errors += 1

    def summary(self) -> dict:
        if not self.samples:
            return {"count": 0, "errors": self.errors}
        s = sorted(self.samples)
        return {
            "count": len(s),
            "errors": self.errors,
            "avg_ms": round(statistics.mean(s), 2),
            "p50_ms": round(s[len(s) // 2], 2),
            "p95_ms": round(s[int(len(s) * 0.95)], 2),
            "p99_ms": round(s[int(len(s) * 0.99)], 2),
            "max_ms": round(s[-1], 2),
        }

@dataclass
class ClientStats:
    static_assets: LatencyBucket = field(default_factory=lambda: LatencyBucket("static"))
    rest_apis: Dict[str, LatencyBucket] = field(default_factory=dict)
    ws_connect: LatencyBucket = field(default_factory=lambda: LatencyBucket("ws_connect"))
    ws_messages: int = 0
    orders: LatencyBucket = field(default_factory=lambda: LatencyBucket("orders"))

    def rest(self, endpoint: str) -> LatencyBucket:
        if endpoint not in self.rest_apis:
            self.rest_apis[endpoint] = LatencyBucket(endpoint)
        return self.rest_apis[endpoint]

def merge_stats(all_stats: List[ClientStats]) -> ClientStats:
    merged = ClientStats()
    for s in all_stats:
        merged.static_assets.samples.extend(s.static_assets.samples)
        merged.static_assets.errors += s.static_assets.errors
        for ep, bucket in s.rest_apis.items():
            merged.rest(ep).samples.extend(bucket.samples)
            merged.rest(ep).errors += bucket.errors
        merged.ws_connect.samples.extend(s.ws_connect.samples)
        merged.ws_connect.errors += s.ws_connect.errors
        merged.ws_messages += s.ws_messages
        merged.orders.samples.extend(s.orders.samples)
        merged.orders.errors += s.orders.errors
    return merged


# ── Static asset fetcher ──────────────────────────────────────────────────────

STATIC_PATHS = [
    "/trading/index.html",
    "/trading/style.css",
    "/trading/app.js",
    "/dashboard/index.html",
    "/dashboard/style.css",
    "/dashboard/app.js",
]

async def fetch_static(session: aiohttp.ClientSession, stats: ClientStats):
    for path in STATIC_PATHS:
        t0 = time.monotonic()
        try:
            async with session.get(f"{BASE_URL}{path}") as resp:
                await resp.read()
                ms = (time.monotonic() - t0) * 1000
                if resp.status == 200:
                    stats.static_assets.record(ms)
                else:
                    stats.static_assets.record_error()
        except Exception:
            stats.static_assets.record_error()


# ── REST API poller (simulates what app.js does every few seconds) ────────────

REST_ENDPOINTS = [
    "/api/pairs",
    f"/api/orderbook/{PAIR}",
    f"/api/trades/{PAIR}",
    f"/api/ticker/{PAIR}",
    "/api/portfolio?user_id=user-1",
    "/api/orders?user_id=user-1&pair_id=BTC-USDT&limit=20&offset=0",
    "/api/metrics",
    "/api/metrics/throughput",
    "/api/metrics/latency",
]

async def poll_rest(session: aiohttp.ClientSession, stats: ClientStats, deadline: float):
    while time.monotonic() < deadline:
        for ep in REST_ENDPOINTS:
            if time.monotonic() >= deadline:
                break
            t0 = time.monotonic()
            try:
                async with session.get(f"{BASE_URL}{ep}") as resp:
                    await resp.read()
                    ms = (time.monotonic() - t0) * 1000
                    if resp.status == 200:
                        stats.rest(ep).record(ms)
                    else:
                        stats.rest(ep).record_error()
            except Exception:
                stats.rest(ep).record_error()
        await asyncio.sleep(2.0)  # UI polls every ~2s


# ── WebSocket client ──────────────────────────────────────────────────────────

async def ws_client(session: aiohttp.ClientSession, stats: ClientStats, path: str, deadline: float):
    t0 = time.monotonic()
    try:
        async with session.ws_connect(f"{WS_BASE}{path}", timeout=5) as ws:
            ms = (time.monotonic() - t0) * 1000
            stats.ws_connect.record(ms)
            while time.monotonic() < deadline:
                try:
                    msg = await asyncio.wait_for(ws.receive(), timeout=2.0)
                    if msg.type == aiohttp.WSMsgType.TEXT:
                        stats.ws_messages += 1
                    elif msg.type in (aiohttp.WSMsgType.CLOSED, aiohttp.WSMsgType.ERROR):
                        break
                except asyncio.TimeoutError:
                    continue
    except Exception:
        stats.ws_connect.record_error()


# ── Order placer (simulates trading activity) ─────────────────────────────────

async def place_orders(session: aiohttp.ClientSession, stats: ClientStats, client_id: int, deadline: float):
    seq = 0
    while time.monotonic() < deadline:
        side = "Buy" if seq % 2 == 0 else "Sell"
        user = f"user-{(client_id % 5) + 1}"
        # Non-crossing IOC orders (no match, pure throughput)
        price = "49000.00" if side == "Buy" else "51000.00"
        body = {
            "user_id": user,
            "pair_id": PAIR,
            "side": side,
            "order_type": "Limit",
            "tif": "IOC",
            "price": price,
            "quantity": "0.00001",
        }
        t0 = time.monotonic()
        try:
            async with session.post(f"{BASE_URL}/api/orders", json=body) as resp:
                await resp.read()
                ms = (time.monotonic() - t0) * 1000
                if resp.status in (200, 201):
                    stats.orders.record(ms)
                else:
                    stats.orders.record_error()
        except Exception:
            stats.orders.record_error()
        seq += 1
        await asyncio.sleep(0.5)  # ~2 orders/sec per client (realistic UI rate)


# ── Single simulated client ───────────────────────────────────────────────────

async def run_client(client_id: int, deadline: float) -> ClientStats:
    stats = ClientStats()
    connector = aiohttp.TCPConnector(limit=10, force_close=False)
    async with aiohttp.ClientSession(connector=connector) as session:
        # Phase 1: initial page load
        await fetch_static(session, stats)

        # Phase 2: run all concurrent activities until deadline
        tasks = [
            asyncio.create_task(poll_rest(session, stats, deadline)),
            asyncio.create_task(ws_client(session, stats, f"/ws/orderbook/{PAIR}", deadline)),
            asyncio.create_task(ws_client(session, stats, f"/ws/trades/{PAIR}", deadline)),
            asyncio.create_task(place_orders(session, stats, client_id, deadline)),
        ]
        await asyncio.gather(*tasks, return_exceptions=True)
    return stats


# ── Server resource snapshot ──────────────────────────────────────────────────

def server_resources():
    try:
        # Find sme-api process
        out = subprocess.check_output(
            ["bash", "-c", "ps aux | grep 'sme-api' | grep -v grep | head -1"],
            text=True
        ).strip()
        if out:
            parts = out.split()
            return {"cpu%": parts[2], "mem%": parts[3], "rss_mb": round(int(parts[5]) / 1024, 1)}
    except Exception:
        pass
    return {}

def connection_count():
    try:
        out = subprocess.check_output(
            ["bash", "-c", "ss -tn state established '( dport = :3001 or sport = :3001 )' | wc -l"],
            text=True
        ).strip()
        return max(0, int(out) - 1)  # subtract header
    except Exception:
        return -1


# ── Main test runner ──────────────────────────────────────────────────────────

async def run_test():
    print("╔══════════════════════════════════════════════════════════════╗")
    print("║       SME UI Client Load Test — Multi-Client Simulator     ║")
    print("╠══════════════════════════════════════════════════════════════╣")
    print(f"║  Server:   {BASE_URL:<48}║")
    print(f"║  Pair:     {PAIR:<48}║")
    print(f"║  Duration: {TEST_DURATION}s per level{' ' * 36}║")
    print(f"║  Clients:  {str(CLIENT_COUNTS):<48}║")
    print("╚══════════════════════════════════════════════════════════════╝")

    all_results = []

    for num_clients in CLIENT_COUNTS:
        print(f"\n{'─' * 62}")
        print(f"  ▶ {num_clients} concurrent UI clients")
        print(f"{'─' * 62}")

        res_before = server_resources()
        deadline = time.monotonic() + TEST_DURATION

        # Launch all clients concurrently
        tasks = [run_client(i, deadline) for i in range(num_clients)]
        client_stats = await asyncio.gather(*tasks)

        res_after = server_resources()
        conns = connection_count()
        merged = merge_stats(client_stats)

        # Print results
        print(f"\n  Static assets (initial load):")
        s = merged.static_assets.summary()
        if s["count"]:
            print(f"    {s['count']} fetches | avg {s['avg_ms']:.1f}ms | p95 {s['p95_ms']:.1f}ms | max {s['max_ms']:.1f}ms | {s['errors']} errors")
        else:
            print(f"    No successful fetches ({s['errors']} errors)")

        print(f"\n  REST API polling:")
        for ep in REST_ENDPOINTS:
            bucket = merged.rest_apis.get(ep)
            if bucket:
                s = bucket.summary()
                short_ep = ep.split("?")[0]
                if s["count"]:
                    print(f"    {short_ep:<35} avg {s['avg_ms']:>7.1f}ms  p95 {s['p95_ms']:>7.1f}ms  max {s['max_ms']:>7.1f}ms  n={s['count']}")
                else:
                    print(f"    {short_ep:<35} FAILED ({s['errors']} errors)")

        print(f"\n  WebSocket connections:")
        ws = merged.ws_connect.summary()
        if ws["count"]:
            print(f"    {ws['count']} connected | avg {ws['avg_ms']:.1f}ms | max {ws['max_ms']:.1f}ms | {ws['errors']} errors")
            print(f"    Total WS messages received: {merged.ws_messages}")
        else:
            print(f"    No connections ({ws['errors']} errors)")

        print(f"\n  Order placement:")
        o = merged.orders.summary()
        if o["count"]:
            print(f"    {o['count']} orders | avg {o['avg_ms']:.1f}ms | p95 {o['p95_ms']:.1f}ms | p99 {o['p99_ms']:.1f}ms | max {o['max_ms']:.1f}ms | {o['errors']} errors")
            rate = o["count"] / TEST_DURATION
            print(f"    Throughput: {rate:.1f} orders/sec")
        else:
            print(f"    No orders placed ({o['errors']} errors)")

        print(f"\n  Server resources:")
        print(f"    CPU: {res_after.get('cpu%', '?')}%  |  RSS: {res_after.get('rss_mb', '?')} MB  |  Connections: {conns}")

        all_results.append({
            "clients": num_clients,
            "static": merged.static_assets.summary(),
            "ws_connect": merged.ws_connect.summary(),
            "ws_messages": merged.ws_messages,
            "orders": merged.orders.summary(),
            "server": res_after,
            "connections": conns,
        })

    # ── Summary table ─────────────────────────────────────────────────────
    print(f"\n{'═' * 62}")
    print("  SUMMARY — Scaling Impact")
    print(f"{'═' * 62}")
    print(f"{'Clients':>8} {'Ord/s':>8} {'Ord avg':>9} {'Ord p95':>9} {'Ord max':>9} {'WS msgs':>8} {'Conns':>6}")
    print(f"{'─' * 62}")
    for r in all_results:
        o = r["orders"]
        rate = o["count"] / TEST_DURATION if o["count"] else 0
        print(f"{r['clients']:>8} {rate:>8.1f} {o.get('avg_ms',0):>8.1f}ms {o.get('p95_ms',0):>8.1f}ms {o.get('max_ms',0):>8.1f}ms {r['ws_messages']:>8} {r['connections']:>6}")

    print(f"\n✅ UI load test complete.")

if __name__ == "__main__":
    asyncio.run(run_test())
