#!/usr/bin/env python3
"""
Comprehensive benchmark — load test + server-side resource profiling.
Captures CPU, memory, connections, FD count, and DB stats alongside client metrics.
"""

import asyncio
import aiohttp
import time
import json
import subprocess
import statistics
from dataclasses import dataclass, field
from typing import Dict, List

import argparse

BASE_URL = "http://localhost:3001"
WS_BASE = "ws://127.0.0.1:3001"
PAIR = "BTC-USDT"
TEST_DURATION = 20  # seconds per phase
CLIENT_COUNTS = [1, 5, 10, 25, 50, 100]

# Connection mode: "rest" = legacy (8 REST polls + 3 WS), "ws" = mux WS + snapshot
CONN_MODE = "rest"

# Run ID prefix for client_order_id tagging — set via --run-id or auto-generated
RUN_ID = None

@dataclass
class LatencyBucket:
    name: str
    samples: List[float] = field(default_factory=list)
    errors: int = 0
    def record(self, ms: float): self.samples.append(ms)
    def record_error(self): self.errors += 1
    def summary(self) -> dict:
        if not self.samples:
            return {"count": 0, "errors": self.errors}
        s = sorted(self.samples)
        return {
            "count": len(s), "errors": self.errors,
            "avg_ms": round(statistics.mean(s), 2),
            "p50_ms": round(s[len(s)//2], 2),
            "p95_ms": round(s[int(len(s)*0.95)], 2),
            "p99_ms": round(s[int(len(s)*0.99)], 2),
            "max_ms": round(s[-1], 2),
        }

@dataclass
class ClientStats:
    static_assets: LatencyBucket = field(default_factory=lambda: LatencyBucket("static"))
    rest_apis: Dict[str, LatencyBucket] = field(default_factory=dict)
    ws_connect: LatencyBucket = field(default_factory=lambda: LatencyBucket("ws_connect"))
    ws_messages: int = 0
    orders: LatencyBucket = field(default_factory=lambda: LatencyBucket("orders"))
    def rest(self, ep): 
        if ep not in self.rest_apis: self.rest_apis[ep] = LatencyBucket(ep)
        return self.rest_apis[ep]

def merge_stats(all_stats):
    m = ClientStats()
    for s in all_stats:
        m.static_assets.samples.extend(s.static_assets.samples)
        m.static_assets.errors += s.static_assets.errors
        for ep, b in s.rest_apis.items():
            m.rest(ep).samples.extend(b.samples)
            m.rest(ep).errors += b.errors
        m.ws_connect.samples.extend(s.ws_connect.samples)
        m.ws_connect.errors += s.ws_connect.errors
        m.ws_messages += s.ws_messages
        m.orders.samples.extend(s.orders.samples)
        m.orders.errors += s.orders.errors
    return m

# ── Server profiling ──────────────────────────────────────────────────────────

def get_sme_pid():
    try:
        out = subprocess.check_output(["pgrep", "-f", "sme-api"], text=True).strip()
        return out.split('\n')[0]
    except: return None

def server_snapshot():
    pid = get_sme_pid()
    snap = {"cpu": "?", "mem_mb": "?", "fds": "?", "threads": "?", "tcp_conns": 0}
    if pid:
        try:
            out = subprocess.check_output(["ps", "-p", pid, "-o", "%cpu,%mem,rss,nlwp", "--no-headers"], text=True).strip()
            parts = out.split()
            snap["cpu"] = f"{float(parts[0]):.1f}%"
            snap["mem_mb"] = round(int(parts[2]) / 1024, 1)
            snap["threads"] = int(parts[3])
        except: pass
        try:
            fds = subprocess.check_output(["ls", f"/proc/{pid}/fd"], text=True).strip().split('\n')
            snap["fds"] = len(fds)
        except: pass
    try:
        out = subprocess.check_output(
            ["bash", "-c", "ss -tn state established '( dport = :3001 or sport = :3001 )' | tail -n+2 | wc -l"],
            text=True).strip()
        snap["tcp_conns"] = int(out)
    except: pass
    return snap

def db_stats():
    try:
        out = subprocess.check_output([
            "docker", "exec", "sme-postgres", "psql", "-U", "sme", "-d", "matching_engine", "-t", "-c",
            "SELECT json_build_object('orders', (SELECT count(*) FROM orders), 'trades', (SELECT count(*) FROM trades), 'pg_conns', (SELECT count(*) FROM pg_stat_activity WHERE datname='matching_engine'));"
        ], text=True).strip()
        return json.loads(out)
    except: return {}

def valkey_stats():
    try:
        out = subprocess.check_output(["docker", "exec", "sme-valkey", "redis-cli", "INFO", "clients"], text=True)
        for line in out.split('\n'):
            if line.startswith('connected_clients:'):
                return {"df_clients": int(line.split(':')[1].strip())}
    except: pass
    return {}

# ── Client simulation ─────────────────────────────────────────────────────────

STATIC_PATHS = ["/trading/index.html", "/trading/style.css", "/trading/app.js"]

REST_ENDPOINTS = [
    "/api/pairs",
    f"/api/orderbook/{PAIR}",
    f"/api/trades/{PAIR}",
    f"/api/ticker/{PAIR}",
    "/api/portfolio?user_id=user-1",
    "/api/metrics",
    "/api/metrics/throughput",
    "/api/metrics/latency",
]

async def fetch_static(session, stats):
    for path in STATIC_PATHS:
        t0 = time.monotonic()
        try:
            async with session.get(f"{BASE_URL}{path}") as resp:
                await resp.read()
                ms = (time.monotonic() - t0) * 1000
                if resp.status == 200: stats.static_assets.record(ms)
                else: stats.static_assets.record_error()
        except: stats.static_assets.record_error()

async def poll_rest(session, stats, deadline):
    while time.monotonic() < deadline:
        for ep in REST_ENDPOINTS:
            if time.monotonic() >= deadline: break
            t0 = time.monotonic()
            try:
                async with session.get(f"{BASE_URL}{ep}") as resp:
                    await resp.read()
                    ms = (time.monotonic() - t0) * 1000
                    if resp.status == 200: stats.rest(ep).record(ms)
                    else: stats.rest(ep).record_error()
            except: stats.rest(ep).record_error()
        await asyncio.sleep(2.0)

async def ws_client(session, stats, path, deadline):
    import websockets
    t0 = time.monotonic()
    try:
        ws = await asyncio.wait_for(
            websockets.connect(f"{WS_BASE}{path}"),
            timeout=5
        )
        ms = (time.monotonic() - t0) * 1000
        stats.ws_connect.record(ms)
        try:
            while time.monotonic() < deadline:
                try:
                    msg = await asyncio.wait_for(ws.recv(), timeout=2.0)
                    if isinstance(msg, str): stats.ws_messages += 1
                except asyncio.TimeoutError: continue
                except Exception: break
        finally:
            await asyncio.wait_for(ws.close(), timeout=1)
    except: stats.ws_connect.record_error()

async def mux_ws_client(session, stats, pair_id, user_id, deadline):
    """Single multiplexed WS connection — replaces 3 separate WS + 8 REST polls."""
    import websockets
    t0 = time.monotonic()
    try:
        ws = await asyncio.wait_for(
            websockets.connect(f"{WS_BASE}/ws/stream"),
            timeout=5
        )
        ms = (time.monotonic() - t0) * 1000
        stats.ws_connect.record(ms)
        try:
            # Subscribe to all channels
            await ws.send(json.dumps({"subscribe": [
                f"orderbook:{pair_id}",
                f"trades:{pair_id}",
                f"ticker:{pair_id}",
                f"orders:{user_id}",
                f"portfolio:{user_id}",
            ]}))
            while time.monotonic() < deadline:
                try:
                    msg = await asyncio.wait_for(ws.recv(), timeout=2.0)
                    if isinstance(msg, str):
                        stats.ws_messages += 1
                except asyncio.TimeoutError:
                    continue
                except Exception:
                    break
        finally:
            await asyncio.wait_for(ws.close(), timeout=1)
    except:
        stats.ws_connect.record_error()

async def fetch_snapshot(session, stats, pair_id):
    """Fetch initial state via single snapshot endpoint (replaces 8 REST calls)."""
    ep = f"/api/snapshot/{pair_id}"
    t0 = time.monotonic()
    try:
        async with session.get(f"{BASE_URL}{ep}") as resp:
            await resp.read()
            ms = (time.monotonic() - t0) * 1000
            if resp.status == 200:
                stats.rest(ep).record(ms)
            else:
                stats.rest(ep).record_error()
    except:
        stats.rest(ep).record_error()

async def place_orders(session, stats, client_id, num_clients, deadline):
    seq = 0
    while time.monotonic() < deadline:
        pattern = seq % 4
        user = f"user-{(client_id % 10) + 1}"
        if pattern == 0:   side, price, tif = "Sell", "70800.00", "GTC"
        elif pattern == 1: side, price, tif = "Buy",  "70800.00", "GTC"
        elif pattern == 2: side, price, tif = "Buy",  "70600.00", "GTC"
        else:              side, price, tif = "Sell", "70600.00", "GTC"
        body = {"user_id": user, "pair_id": PAIR, "side": side,
                "order_type": "Limit", "tif": tif, "price": price, "quantity": "0.00100"}
        if RUN_ID:
            body["client_order_id"] = f"{RUN_ID}-n{num_clients}-c{client_id}-{seq}"
        t0 = time.monotonic()
        try:
            async with session.post(f"{BASE_URL}/api/orders", json=body) as resp:
                await resp.read()
                ms = (time.monotonic() - t0) * 1000
                if resp.status in (200, 201, 202): stats.orders.record(ms)
                else: stats.orders.record_error()
        except: stats.orders.record_error()
        seq += 1
        await asyncio.sleep(0.5)

async def run_client(client_id, num_clients, deadline):
    stats = ClientStats()
    connector = aiohttp.TCPConnector(limit=10, force_close=False)
    async with aiohttp.ClientSession(connector=connector) as session:
        await fetch_static(session, stats)
        user_id = f"user-{(client_id % 10) + 1}"

        if CONN_MODE == "ws":
            # WS-only mode: 1 snapshot + 1 mux WS + orders
            await fetch_snapshot(session, stats, PAIR)
            tasks = [
                asyncio.create_task(mux_ws_client(session, stats, PAIR, user_id, deadline)),
                asyncio.create_task(place_orders(session, stats, client_id, num_clients, deadline)),
            ]
        else:
            # Legacy mode: 8 REST polls + 3 separate WS + orders
            tasks = [
                asyncio.create_task(poll_rest(session, stats, deadline)),
                asyncio.create_task(ws_client(session, stats, f"/ws/orderbook/{PAIR}", deadline)),
                asyncio.create_task(ws_client(session, stats, f"/ws/trades/{PAIR}", deadline)),
                asyncio.create_task(ws_client(session, stats, f"/ws/orders/{user_id}", deadline)),
                asyncio.create_task(place_orders(session, stats, client_id, num_clients, deadline)),
            ]
        await asyncio.gather(*tasks, return_exceptions=True)
    return stats

# ── Main runner ───────────────────────────────────────────────────────────────

async def run_benchmark():
    mode_label = "WS-only (snapshot + mux WS)" if CONN_MODE == "ws" else "REST+WS (8 polls + 3 WS)"
    print("╔════════════════════════════════════════════════════════════════════╗")
    print("║          SME Comprehensive Benchmark — Server + Client           ║")
    print("╠════════════════════════════════════════════════════════════════════╣")
    print(f"║  Server:   {BASE_URL:<55}║")
    print(f"║  Mode:     {mode_label:<55}║")
    print(f"║  Run ID:   {RUN_ID:<55}║")
    print(f"║  Duration: {TEST_DURATION}s per level, clients: {CLIENT_COUNTS!s:<27}║")
    print("╚════════════════════════════════════════════════════════════════════╝")

    # Baseline server state
    base_db = db_stats()
    print(f"\n  Baseline: {base_db.get('orders',0)} orders, {base_db.get('trades',0)} trades in DB")

    all_results = []

    for num_clients in CLIENT_COUNTS:
        print(f"\n{'━' * 70}")
        print(f"  ▶ {num_clients} concurrent UI clients — {TEST_DURATION}s")
        print(f"{'━' * 70}")

        snap_before = server_snapshot()
        db_before = db_stats()
        df_before = valkey_stats()

        deadline = time.monotonic() + TEST_DURATION

        # Sample server stats during test (every 2s)
        server_samples = []
        async def sample_server():
            while time.monotonic() < deadline:
                await asyncio.sleep(2)
                server_samples.append(server_snapshot())

        sampler = asyncio.create_task(sample_server())
        tasks = [run_client(i, num_clients, deadline) for i in range(num_clients)]
        client_stats = await asyncio.gather(*tasks)
        await sampler

        snap_after = server_snapshot()
        db_after = db_stats()
        df_after = valkey_stats()
        merged = merge_stats(client_stats)

        # Peak server metrics
        peak_cpu = max([s.get("cpu","0%").rstrip('%') for s in server_samples] or ["0"], key=lambda x: float(x) if x != '?' else 0)
        peak_mem = max([s.get("mem_mb",0) for s in server_samples] or [0])
        peak_conns = max([s.get("tcp_conns",0) for s in server_samples] or [0])
        peak_fds = max([s.get("fds",0) for s in server_samples] or [0])
        peak_threads = max([s.get("threads",0) for s in server_samples] or [0])

        new_orders = db_after.get('orders',0) - db_before.get('orders',0)
        new_trades = db_after.get('trades',0) - db_before.get('trades',0)

        # Print results
        o = merged.orders.summary()
        ord_rate = o["count"] / TEST_DURATION if o["count"] else 0
        trade_rate = new_trades / TEST_DURATION if new_trades else 0

        print(f"\n  📊 Throughput:")
        print(f"    Orders:  {o['count']:>6} placed ({ord_rate:.1f}/sec) | {o['errors']} errors")
        print(f"    Trades:  {new_orders:>6} new orders in DB | {new_trades} trades ({trade_rate:.1f}/sec)")
        print(f"    WS msgs: {merged.ws_messages:>6} delivered to {num_clients*3} connections")

        print(f"\n  ⏱  Order Latency:")
        if o["count"]:
            print(f"    avg={o['avg_ms']:.1f}ms  p50={o['p50_ms']:.1f}ms  p95={o['p95_ms']:.1f}ms  p99={o['p99_ms']:.1f}ms  max={o['max_ms']:.1f}ms")

        print(f"\n  🌐 REST API Latency (avg / p95 / max):")
        for ep in REST_ENDPOINTS:
            b = merged.rest_apis.get(ep)
            if b:
                s = b.summary()
                short = ep.split("?")[0]
                if s["count"]:
                    print(f"    {short:<35} {s['avg_ms']:>7.1f}  {s['p95_ms']:>7.1f}  {s['max_ms']:>7.1f}ms  n={s['count']}")

        print(f"\n  🔌 Server Resources (peak during test):")
        print(f"    CPU: {peak_cpu}%  |  Mem: {peak_mem} MB  |  Threads: {peak_threads}")
        print(f"    TCP conns: {peak_conns}  |  FDs: {peak_fds}  |  PG conns: {db_after.get('pg_conns','?')}")
        print(f"    Valkey clients: {df_after.get('df_clients','?')}")

        all_results.append({
            "clients": num_clients,
            "orders": o,
            "new_trades": new_trades,
            "ord_rate": round(ord_rate, 1),
            "trade_rate": round(trade_rate, 1),
            "ws_messages": merged.ws_messages,
            "peak_cpu": peak_cpu,
            "peak_mem": peak_mem,
            "peak_conns": peak_conns,
            "peak_fds": peak_fds,
            "peak_threads": peak_threads,
            "pg_conns": db_after.get("pg_conns", "?"),
            "df_clients": df_after.get("df_clients", "?"),
            "rest": {ep.split("?")[0]: merged.rest_apis[ep].summary() for ep in REST_ENDPOINTS if ep in merged.rest_apis},
        })

    # ── Summary table ─────────────────────────────────────────────────────
    print(f"\n{'═' * 70}")
    print("  BENCHMARK SUMMARY")
    print(f"{'═' * 70}")
    print(f"{'Clients':>8} {'Ord/s':>7} {'Trd/s':>7} {'Ord avg':>9} {'Ord p95':>9} {'WS msgs':>8} {'CPU':>6} {'Mem MB':>7} {'PG':>4} {'DF':>4} {'FDs':>5}")
    print(f"{'─' * 70}")
    for r in all_results:
        o = r["orders"]
        print(f"{r['clients']:>8} {r['ord_rate']:>7.1f} {r['trade_rate']:>7.1f} {o.get('avg_ms',0):>8.1f}ms {o.get('p95_ms',0):>8.1f}ms {r['ws_messages']:>8} {r['peak_cpu']:>5}% {r['peak_mem']:>7} {r['pg_conns']:>4} {r['df_clients']:>4} {r['peak_fds']:>5}")

    # ── Bottleneck analysis ───────────────────────────────────────────────
    print(f"\n{'═' * 70}")
    print("  BOTTLENECK ANALYSIS")
    print(f"{'═' * 70}")

    # Collect degradation ratios
    if len(all_results) >= 2:
        r1 = all_results[0]  # 1 client baseline
        rN = all_results[-1]  # max clients

        bottlenecks = []

        # Check each REST endpoint degradation
        for ep, s1 in r1.get("rest", {}).items():
            sN = rN.get("rest", {}).get(ep, {})
            if s1.get("avg_ms") and sN.get("avg_ms") and s1["avg_ms"] > 0:
                ratio = sN["avg_ms"] / s1["avg_ms"]
                bottlenecks.append((ratio, ep, s1["avg_ms"], sN["avg_ms"], sN.get("p95_ms", 0)))

        # Order latency degradation
        o1 = r1["orders"]
        oN = rN["orders"]
        if o1.get("avg_ms") and oN.get("avg_ms") and o1["avg_ms"] > 0:
            ratio = oN["avg_ms"] / o1["avg_ms"]
            bottlenecks.append((ratio, "POST /api/orders", o1["avg_ms"], oN["avg_ms"], oN.get("p95_ms", 0)))

        # Throughput ceiling
        max_rate = max(r["ord_rate"] for r in all_results)
        final_rate = all_results[-1]["ord_rate"]
        if final_rate < max_rate * 0.95:
            bottlenecks.append((max_rate / max(final_rate, 1), f"THROUGHPUT CEILING: peak {max_rate}/s, dropped to {final_rate}/s at {all_results[-1]['clients']} clients", 0, 0, 0))

        bottlenecks.sort(reverse=True)

        for i, (ratio, name, base, scaled, p95) in enumerate(bottlenecks[:5], 1):
            if base > 0:
                print(f"\n  #{i}  {name}")
                print(f"      1 client: {base:.1f}ms → {all_results[-1]['clients']} clients: {scaled:.1f}ms (p95: {p95:.1f}ms)")
                print(f"      Degradation: {ratio:.1f}x slower")
            else:
                print(f"\n  #{i}  {name}")
                print(f"      Degradation: {ratio:.1f}x")

    print(f"\n✅ Benchmark complete.")

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="SME Benchmark")
    parser.add_argument("--ws", action="store_true", help="WS-only mode: snapshot + mux WS (fewer TCP connections)")
    parser.add_argument("--duration", type=int, default=TEST_DURATION, help="Seconds per client level")
    parser.add_argument("--clients", type=str, default=None, help="Comma-separated client counts (e.g. '1,10,50,100')")
    parser.add_argument("--run-id", type=str, default=None,
                        help="Run ID prefix for client_order_id (default: auto-generated bench-YYYYMMDD-HHMM)")
    args = parser.parse_args()

    if args.ws:
        CONN_MODE = "ws"
    if args.duration:
        TEST_DURATION = args.duration
    if args.clients:
        CLIENT_COUNTS = [int(x.strip()) for x in args.clients.split(",")]

    # Auto-generate RUN_ID if not provided
    from datetime import datetime
    RUN_ID = args.run_id or f"bench-{datetime.utcnow().strftime('%Y%m%d-%H%M')}"

    asyncio.run(run_benchmark())
