#!/usr/bin/env python3
"""
Benchmark v2 — tests both legacy (8 REST + 3 WS) and optimized (snapshot + mux WS) modes.
Compares connection count, latency, and throughput.
"""

import asyncio
import aiohttp
import time
import json
import subprocess
import statistics
from dataclasses import dataclass, field
from typing import Dict, List

BASE_URL = "http://localhost:3001"
WS_BASE = "ws://127.0.0.1:3001"
PAIR = "BTC-USDT"
TEST_DURATION = 20  # seconds per phase
CLIENT_COUNTS = [1, 5, 10, 25, 50, 100]

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

def get_sme_pids():
    """Get PIDs for both gateway and worker."""
    pids = {}
    for name in ["sme-gateway", "sme-api"]:
        try:
            out = subprocess.check_output(["pgrep", "-f", name], text=True).strip()
            pids[name] = out.split('\n')[0]
        except: pass
    return pids

def server_snapshot():
    pids = get_sme_pids()
    snap = {"cpu": "0.0", "mem_mb": 0, "fds": 0, "threads": 0, "tcp_conns": 0}
    total_cpu = 0.0
    total_mem = 0
    total_fds = 0
    total_threads = 0
    for name, pid in pids.items():
        try:
            out = subprocess.check_output(["ps", "-p", pid, "-o", "%cpu,%mem,rss,nlwp", "--no-headers"], text=True).strip()
            parts = out.split()
            total_cpu += float(parts[0])
            total_mem += int(parts[2])
            total_threads += int(parts[3])
        except: pass
        try:
            fds = subprocess.check_output(["ls", f"/proc/{pid}/fd"], text=True).strip().split('\n')
            total_fds += len(fds)
        except: pass
    snap["cpu"] = f"{total_cpu:.1f}"
    snap["mem_mb"] = round(total_mem / 1024, 1)
    snap["fds"] = total_fds
    snap["threads"] = total_threads
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

def dragonfly_stats():
    try:
        out = subprocess.check_output(["docker", "exec", "sme-dragonfly", "redis-cli", "INFO", "clients"], text=True)
        for line in out.split('\n'):
            if line.startswith('connected_clients:'):
                return {"df_clients": int(line.split(':')[1].strip())}
    except: pass
    return {}

# ── Client simulation — LEGACY MODE (8 REST + 3 WS) ──────────────────────────

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

async def poll_rest_legacy(session, stats, deadline):
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

async def poll_snapshot(session, stats, deadline):
    """Optimized mode: single /api/snapshot request replaces 8 REST polls."""
    ep = f"/api/snapshot/{PAIR}"
    while time.monotonic() < deadline:
        t0 = time.monotonic()
        try:
            async with session.get(f"{BASE_URL}{ep}") as resp:
                await resp.read()
                ms = (time.monotonic() - t0) * 1000
                if resp.status == 200: stats.rest(ep).record(ms)
                else: stats.rest(ep).record_error()
        except: stats.rest(ep).record_error()
        await asyncio.sleep(2.0)

async def ws_client_legacy(session, stats, path, deadline):
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

async def ws_client_mux(session, stats, deadline):
    """Optimized mode: single multiplexed WS replaces 3 separate connections."""
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
            sub = json.dumps({"subscribe": [
                f"orderbook:{PAIR}",
                f"trades:{PAIR}",
                f"ticker:{PAIR}",
            ]})
            await ws.send(sub)
            while time.monotonic() < deadline:
                try:
                    msg = await asyncio.wait_for(ws.recv(), timeout=2.0)
                    if isinstance(msg, str): stats.ws_messages += 1
                except asyncio.TimeoutError: continue
                except Exception: break
        finally:
            await asyncio.wait_for(ws.close(), timeout=1)
    except: stats.ws_connect.record_error()

async def place_orders(session, stats, client_id, deadline):
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

async def run_client_legacy(client_id, deadline):
    stats = ClientStats()
    connector = aiohttp.TCPConnector(limit=10, force_close=False)
    async with aiohttp.ClientSession(connector=connector) as session:
        await fetch_static(session, stats)
        user_id = f"user-{(client_id % 10) + 1}"
        tasks = [
            asyncio.create_task(poll_rest_legacy(session, stats, deadline)),
            asyncio.create_task(ws_client_legacy(session, stats, f"/ws/orderbook/{PAIR}", deadline)),
            asyncio.create_task(ws_client_legacy(session, stats, f"/ws/trades/{PAIR}", deadline)),
            asyncio.create_task(ws_client_legacy(session, stats, f"/ws/orders/{user_id}", deadline)),
            asyncio.create_task(place_orders(session, stats, client_id, deadline)),
        ]
        await asyncio.gather(*tasks, return_exceptions=True)
    return stats

async def run_client_optimized(client_id, deadline):
    stats = ClientStats()
    connector = aiohttp.TCPConnector(limit=5, force_close=False)
    async with aiohttp.ClientSession(connector=connector) as session:
        await fetch_static(session, stats)
        tasks = [
            asyncio.create_task(poll_snapshot(session, stats, deadline)),
            asyncio.create_task(ws_client_mux(session, stats, deadline)),
            asyncio.create_task(place_orders(session, stats, client_id, deadline)),
        ]
        await asyncio.gather(*tasks, return_exceptions=True)
    return stats

# ── Benchmark runner ──────────────────────────────────────────────────────────

async def run_level(num_clients, mode, deadline_offset):
    deadline = time.monotonic() + deadline_offset

    snap_before = server_snapshot()
    db_before = db_stats()
    df_before = dragonfly_stats()

    server_samples = []
    async def sample_server():
        while time.monotonic() < deadline:
            await asyncio.sleep(2)
            server_samples.append(server_snapshot())

    sampler = asyncio.create_task(sample_server())

    if mode == "legacy":
        tasks = [run_client_legacy(i, deadline) for i in range(num_clients)]
    else:
        tasks = [run_client_optimized(i, deadline) for i in range(num_clients)]

    client_stats = await asyncio.gather(*tasks)
    await sampler

    db_after = db_stats()
    df_after = dragonfly_stats()
    merged = merge_stats(client_stats)

    # Peak server metrics
    peak_cpu = max([float(s.get("cpu","0")) for s in server_samples] or [0])
    peak_mem = max([s.get("mem_mb",0) for s in server_samples] or [0])
    peak_conns = max([s.get("tcp_conns",0) for s in server_samples] or [0])
    peak_fds = max([s.get("fds",0) for s in server_samples] or [0])

    new_orders = db_after.get('orders',0) - db_before.get('orders',0)
    new_trades = db_after.get('trades',0) - db_before.get('trades',0)

    o = merged.orders.summary()
    ord_rate = o["count"] / TEST_DURATION if o["count"] else 0
    trade_rate = new_trades / TEST_DURATION if new_trades else 0

    return {
        "clients": num_clients,
        "mode": mode,
        "orders": o,
        "new_trades": new_trades,
        "ord_rate": round(ord_rate, 1),
        "trade_rate": round(trade_rate, 1),
        "ws_messages": merged.ws_messages,
        "peak_cpu": f"{peak_cpu:.1f}",
        "peak_mem": peak_mem,
        "peak_conns": peak_conns,
        "peak_fds": peak_fds,
        "pg_conns": db_after.get("pg_conns", "?"),
        "df_clients": df_after.get("df_clients", "?"),
        "rest": {ep.split("?")[0]: merged.rest_apis[ep].summary() for ep in merged.rest_apis},
    }

async def run_benchmark():
    print("╔════════════════════════════════════════════════════════════════════════╗")
    print("║       SME Benchmark v2 — Legacy vs Optimized Comparison              ║")
    print("╠════════════════════════════════════════════════════════════════════════╣")
    print(f"║  Server:   {BASE_URL:<59}║")
    print(f"║  Duration: {TEST_DURATION}s per level, clients: {CLIENT_COUNTS!s:<31}║")
    print(f"║  Modes:    legacy (8 REST + 3 WS) vs optimized (snapshot + mux WS)  ║")
    print("╚════════════════════════════════════════════════════════════════════════╝")

    base_db = db_stats()
    print(f"\n  Baseline: {base_db.get('orders',0)} orders, {base_db.get('trades',0)} trades in DB")

    legacy_results = []
    opt_results = []

    for num_clients in CLIENT_COUNTS:
        for mode in ["legacy", "optimized"]:
            label = "LEGACY (8 REST + 3 WS)" if mode == "legacy" else "OPTIMIZED (snapshot + mux WS)"
            print(f"\n{'━' * 74}")
            print(f"  ▶ {num_clients} clients × {label} — {TEST_DURATION}s")
            print(f"{'━' * 74}")

            result = await run_level(num_clients, mode, TEST_DURATION)

            o = result["orders"]
            print(f"  Orders: {o['count']} ({result['ord_rate']}/s) | Trades: {result['new_trades']} ({result['trade_rate']}/s)")
            if o.get("avg_ms"):
                print(f"  Order latency: avg={o['avg_ms']:.1f}ms  p95={o['p95_ms']:.1f}ms  p99={o['p99_ms']:.1f}ms")
            print(f"  TCP conns: {result['peak_conns']}  |  WS msgs: {result['ws_messages']}  |  DF: {result['df_clients']}  |  CPU: {result['peak_cpu']}%")

            # Print REST details
            for ep, s in result["rest"].items():
                if s.get("avg_ms"):
                    print(f"    {ep:<40} avg={s['avg_ms']:.1f}ms  p95={s['p95_ms']:.1f}ms")

            if mode == "legacy":
                legacy_results.append(result)
            else:
                opt_results.append(result)

            # Brief pause between modes to let connections drain
            await asyncio.sleep(2)

    # ── Comparison table ──────────────────────────────────────────────────
    print(f"\n{'═' * 90}")
    print("  HEAD-TO-HEAD COMPARISON: LEGACY vs OPTIMIZED")
    print(f"{'═' * 90}")
    print(f"{'Clients':>8} │{'── LEGACY ──':^30}│{'── OPTIMIZED ──':^30}│{'─ DELTA ─':^16}")
    print(f"{'':>8} │{'Ord/s':>7} {'Ord p95':>8} {'TCP':>5} {'WS':>6} │{'Ord/s':>7} {'Ord p95':>8} {'TCP':>5} {'WS':>6} │{'Ord/s':>7} {'TCP':>5}")
    print(f"{'─' * 90}")

    for leg, opt in zip(legacy_results, opt_results):
        lo, oo = leg["orders"], opt["orders"]
        lp95 = lo.get("p95_ms", 0)
        op95 = oo.get("p95_ms", 0)
        ord_delta = opt["ord_rate"] - leg["ord_rate"]
        tcp_delta = opt["peak_conns"] - leg["peak_conns"]
        ord_sign = "+" if ord_delta > 0 else ""
        tcp_sign = "+" if tcp_delta > 0 else ""
        print(f"{leg['clients']:>8} │{leg['ord_rate']:>7.1f} {lp95:>7.1f}ms {leg['peak_conns']:>5} {leg['ws_messages']:>6} │"
              f"{opt['ord_rate']:>7.1f} {op95:>7.1f}ms {opt['peak_conns']:>5} {opt['ws_messages']:>6} │"
              f"{ord_sign}{ord_delta:>6.1f} {tcp_sign}{tcp_delta:>4}")

    # ── Connection reduction analysis ─────────────────────────────────────
    if legacy_results and opt_results:
        l100 = legacy_results[-1]
        o100 = opt_results[-1]
        tcp_reduction = ((l100["peak_conns"] - o100["peak_conns"]) / max(l100["peak_conns"], 1)) * 100
        print(f"\n  📉 TCP connection reduction at {l100['clients']} clients: "
              f"{l100['peak_conns']} → {o100['peak_conns']} ({tcp_reduction:.0f}% fewer)")

        lo, oo = l100["orders"], o100["orders"]
        if lo.get("avg_ms") and oo.get("avg_ms"):
            lat_improvement = ((lo["avg_ms"] - oo["avg_ms"]) / lo["avg_ms"]) * 100
            print(f"  ⚡ Order latency improvement: {lo['avg_ms']:.1f}ms → {oo['avg_ms']:.1f}ms ({lat_improvement:.0f}% faster)")

        if l100["ord_rate"] and o100["ord_rate"]:
            thr_improvement = ((o100["ord_rate"] - l100["ord_rate"]) / l100["ord_rate"]) * 100
            print(f"  📈 Throughput change: {l100['ord_rate']}/s → {o100['ord_rate']}/s ({thr_improvement:+.0f}%)")

    print(f"\n✅ Benchmark v2 complete.")

if __name__ == "__main__":
    asyncio.run(run_benchmark())
