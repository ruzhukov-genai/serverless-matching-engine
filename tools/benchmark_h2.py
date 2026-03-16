#!/usr/bin/env python3
"""
HTTP/2 multiplexed benchmark — tests the real production profile.
Browsers use HTTP/2 which multiplexes requests over fewer TCP connections.
This benchmark uses httpx with h2 to simulate that behavior.

Compares: HTTP/1.1 (aiohttp) vs HTTP/2 (httpx) with same workload.
"""

import asyncio
import time
import json
import subprocess
import statistics
import httpx
from dataclasses import dataclass, field
from typing import Dict, List

BASE_URL = "http://localhost:3001"
WS_BASE = "ws://127.0.0.1:3001"
PAIR = "BTC-USDT"
TEST_DURATION = 20
CLIENT_COUNTS = [1, 10, 25, 50, 100]

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
    rest_apis: Dict[str, LatencyBucket] = field(default_factory=dict)
    ws_messages: int = 0
    orders: LatencyBucket = field(default_factory=lambda: LatencyBucket("orders"))
    def rest(self, ep):
        if ep not in self.rest_apis: self.rest_apis[ep] = LatencyBucket(ep)
        return self.rest_apis[ep]

def merge_stats(all_stats):
    m = ClientStats()
    for s in all_stats:
        for ep, b in s.rest_apis.items():
            m.rest(ep).samples.extend(b.samples)
            m.rest(ep).errors += b.errors
        m.ws_messages += s.ws_messages
        m.orders.samples.extend(s.orders.samples)
        m.orders.errors += s.orders.errors
    return m

# ── Server profiling ──────────────────────────────────────────────────────────

def server_snapshot():
    snap = {"tcp_conns": 0}
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
            "SELECT json_build_object('orders', (SELECT count(*) FROM orders), 'trades', (SELECT count(*) FROM trades));"
        ], text=True).strip()
        return json.loads(out)
    except: return {}

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

# ── HTTP/2 client (httpx) ─────────────────────────────────────────────────────

async def poll_rest_h2(client, stats, deadline):
    """HTTP/2 multiplexed REST polling — all requests share one TCP connection."""
    while time.monotonic() < deadline:
        for ep in REST_ENDPOINTS:
            if time.monotonic() >= deadline: break
            t0 = time.monotonic()
            try:
                resp = await client.get(f"{BASE_URL}{ep}")
                ms = (time.monotonic() - t0) * 1000
                if resp.status_code == 200:
                    stats.rest(ep).record(ms)
                else:
                    stats.rest(ep).record_error()
            except:
                stats.rest(ep).record_error()
        await asyncio.sleep(2.0)

async def poll_snapshot_h2(client, stats, deadline):
    """Optimized: single /api/snapshot replaces 8 REST polls."""
    ep = f"/api/snapshot/{PAIR}"
    while time.monotonic() < deadline:
        t0 = time.monotonic()
        try:
            resp = await client.get(f"{BASE_URL}{ep}")
            ms = (time.monotonic() - t0) * 1000
            if resp.status_code == 200:
                stats.rest(ep).record(ms)
            else:
                stats.rest(ep).record_error()
        except:
            stats.rest(ep).record_error()
        await asyncio.sleep(2.0)

async def place_orders_h2(client, stats, client_id, deadline):
    seq = 0
    while time.monotonic() < deadline:
        pattern = seq % 4
        user = f"user-{(client_id % 10) + 1}"
        if pattern == 0:   side, price = "Sell", "70800.00"
        elif pattern == 1: side, price = "Buy",  "70800.00"
        elif pattern == 2: side, price = "Buy",  "70600.00"
        else:              side, price = "Sell", "70600.00"
        body = {"user_id": user, "pair_id": PAIR, "side": side,
                "order_type": "Limit", "tif": "GTC", "price": price, "quantity": "0.00100"}
        t0 = time.monotonic()
        try:
            resp = await client.post(f"{BASE_URL}/api/orders", json=body)
            ms = (time.monotonic() - t0) * 1000
            if resp.status_code in (200, 201, 202):
                stats.orders.record(ms)
            else:
                stats.orders.record_error()
        except:
            stats.orders.record_error()
        seq += 1
        await asyncio.sleep(0.5)

async def ws_client_mux(stats, deadline):
    """Single multiplexed WS connection."""
    import websockets
    try:
        ws_conn = await asyncio.wait_for(
            websockets.connect(f"{WS_BASE}/ws/stream"),
            timeout=5
        )
        sub = json.dumps({"subscribe": [
            f"orderbook:{PAIR}", f"trades:{PAIR}", f"ticker:{PAIR}",
        ]})
        await ws_conn.send(sub)
        try:
            while time.monotonic() < deadline:
                try:
                    msg = await asyncio.wait_for(ws_conn.recv(), timeout=2.0)
                    if isinstance(msg, str): stats.ws_messages += 1
                except asyncio.TimeoutError: continue
                except Exception: break
        finally:
            await asyncio.wait_for(ws_conn.close(), timeout=1)
    except: pass

# ── Test modes ────────────────────────────────────────────────────────────────

async def run_client_h2_legacy(client_id, deadline, shared_client):
    """HTTP/2 + 8 REST endpoints + mux WS."""
    stats = ClientStats()
    tasks = [
        asyncio.create_task(poll_rest_h2(shared_client, stats, deadline)),
        asyncio.create_task(ws_client_mux(stats, deadline)),
        asyncio.create_task(place_orders_h2(shared_client, stats, client_id, deadline)),
    ]
    await asyncio.gather(*tasks, return_exceptions=True)
    return stats

async def run_client_h2_optimized(client_id, deadline, shared_client):
    """HTTP/2 + snapshot API + mux WS. Minimal connections."""
    stats = ClientStats()
    tasks = [
        asyncio.create_task(poll_snapshot_h2(shared_client, stats, deadline)),
        asyncio.create_task(ws_client_mux(stats, deadline)),
        asyncio.create_task(place_orders_h2(shared_client, stats, client_id, deadline)),
    ]
    await asyncio.gather(*tasks, return_exceptions=True)
    return stats

# ── Benchmark runner ──────────────────────────────────────────────────────────

async def run_level(num_clients, mode, test_duration):
    deadline = time.monotonic() + test_duration
    db_before = db_stats()

    server_samples = []
    async def sample_server():
        while time.monotonic() < deadline:
            await asyncio.sleep(2)
            server_samples.append(server_snapshot())

    sampler = asyncio.create_task(sample_server())

    # HTTP/2 client — shared across all "clients" to simulate browser multiplexing
    # Each browser shares one HTTP/2 connection to the server
    # With N clients, we use ceil(N/10) shared connections (each h2 conn handles ~10 streams)
    num_h2_conns = max(1, (num_clients + 9) // 10)
    clients = []
    for _ in range(num_h2_conns):
        c = httpx.AsyncClient(http2=True, limits=httpx.Limits(max_connections=1, max_keepalive_connections=1))
        clients.append(c)

    if mode == "h2_legacy":
        tasks = [run_client_h2_legacy(i, deadline, clients[i % len(clients)]) for i in range(num_clients)]
    else:
        tasks = [run_client_h2_optimized(i, deadline, clients[i % len(clients)]) for i in range(num_clients)]

    client_stats = await asyncio.gather(*tasks)
    await sampler

    for c in clients:
        await c.aclose()

    db_after = db_stats()
    merged = merge_stats(client_stats)

    peak_conns = max([s.get("tcp_conns", 0) for s in server_samples] or [0])
    new_orders = db_after.get('orders', 0) - db_before.get('orders', 0)
    new_trades = db_after.get('trades', 0) - db_before.get('trades', 0)

    o = merged.orders.summary()
    ord_rate = o["count"] / test_duration if o["count"] else 0
    trade_rate = new_trades / test_duration if new_trades else 0

    return {
        "clients": num_clients,
        "mode": mode,
        "orders": o,
        "ord_rate": round(ord_rate, 1),
        "trade_rate": round(trade_rate, 1),
        "new_trades": new_trades,
        "ws_messages": merged.ws_messages,
        "peak_conns": peak_conns,
        "h2_conns": num_h2_conns,
        "rest": {ep.split("?")[0]: merged.rest_apis[ep].summary() for ep in merged.rest_apis},
    }

async def run_benchmark():
    print("╔════════════════════════════════════════════════════════════════════════╗")
    print("║      SME HTTP/2 Benchmark — Multiplexed Connection Testing           ║")
    print("╠════════════════════════════════════════════════════════════════════════╣")
    print(f"║  Server:   {BASE_URL:<59}║")
    print(f"║  Duration: {TEST_DURATION}s per level, clients: {CLIENT_COUNTS!s:<31}║")
    print(f"║  Modes:    h2 + 8 REST  vs  h2 + snapshot + mux WS                  ║")
    print("╚════════════════════════════════════════════════════════════════════════╝")

    base_db = db_stats()
    print(f"\n  Baseline: {base_db.get('orders',0)} orders, {base_db.get('trades',0)} trades in DB")

    h2_legacy = []
    h2_opt = []

    for num_clients in CLIENT_COUNTS:
        for mode in ["h2_legacy", "h2_optimized"]:
            label = "H2 + 8 REST + mux WS" if mode == "h2_legacy" else "H2 + snapshot + mux WS"
            print(f"\n{'━' * 74}")
            print(f"  ▶ {num_clients} clients × {label} — {TEST_DURATION}s")
            print(f"{'━' * 74}")

            result = await run_level(num_clients, mode, TEST_DURATION)
            o = result["orders"]

            print(f"  Orders: {o['count']} ({result['ord_rate']}/s) | Trades: {result['new_trades']} ({result['trade_rate']}/s)")
            if o.get("avg_ms"):
                print(f"  Order latency: avg={o['avg_ms']:.1f}ms  p95={o['p95_ms']:.1f}ms  p99={o['p99_ms']:.1f}ms")
            print(f"  TCP conns: {result['peak_conns']}  |  H2 conns: {result['h2_conns']}  |  WS msgs: {result['ws_messages']}")

            for ep, s in result["rest"].items():
                if s.get("avg_ms"):
                    print(f"    {ep:<40} avg={s['avg_ms']:.1f}ms  p95={s['p95_ms']:.1f}ms")

            if mode == "h2_legacy":
                h2_legacy.append(result)
            else:
                h2_opt.append(result)

            await asyncio.sleep(2)

    # ── Comparison table ──────────────────────────────────────────────────
    print(f"\n{'═' * 90}")
    print("  HTTP/2 BENCHMARK COMPARISON")
    print(f"{'═' * 90}")
    print(f"{'Clients':>8} │{'── H2 + 8 REST ──':^28}│{'── H2 + Snapshot ──':^28}│{'─ DELTA ─':^14}")
    print(f"{'':>8} │{'Ord/s':>7} {'p95':>8} {'TCP':>5} {'WS':>6} │{'Ord/s':>7} {'p95':>8} {'TCP':>5} {'WS':>6} │{'Ord/s':>7} {'TCP':>5}")
    print(f"{'─' * 90}")

    for leg, opt in zip(h2_legacy, h2_opt):
        lo, oo = leg["orders"], opt["orders"]
        lp95 = lo.get("p95_ms", 0)
        op95 = oo.get("p95_ms", 0)
        ord_delta = opt["ord_rate"] - leg["ord_rate"]
        tcp_delta = opt["peak_conns"] - leg["peak_conns"]
        print(f"{leg['clients']:>8} │{leg['ord_rate']:>7.1f} {lp95:>7.1f}ms {leg['peak_conns']:>5} {leg['ws_messages']:>6} │"
              f"{opt['ord_rate']:>7.1f} {op95:>7.1f}ms {opt['peak_conns']:>5} {opt['ws_messages']:>6} │"
              f"{'+' if ord_delta > 0 else ''}{ord_delta:>6.1f} {'+' if tcp_delta > 0 else ''}{tcp_delta:>4}")

    # Final analysis
    if h2_legacy and h2_opt:
        l = h2_legacy[-1]
        o = h2_opt[-1]
        print(f"\n  📉 TCP at {l['clients']} clients: {l['peak_conns']} → {o['peak_conns']} "
              f"({((l['peak_conns']-o['peak_conns'])/max(l['peak_conns'],1))*100:.0f}% fewer)")
        lo, oo = l["orders"], o["orders"]
        if lo.get("avg_ms") and oo.get("avg_ms"):
            print(f"  ⚡ Order latency: {lo['avg_ms']:.1f}ms → {oo['avg_ms']:.1f}ms "
                  f"({((lo['avg_ms']-oo['avg_ms'])/lo['avg_ms'])*100:.0f}% faster)")

    print(f"\n✅ HTTP/2 benchmark complete.")

if __name__ == "__main__":
    asyncio.run(run_benchmark())
