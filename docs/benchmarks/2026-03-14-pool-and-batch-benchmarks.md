# Benchmark Report: Pool Tuning + Batch Trade Persistence (2026-03-14)

## Environment
- **Host:** Ubuntu server (local Docker Compose)
- **Valkey:** Docker container, `--cache_mode=true`
- **PostgreSQL:** Docker container, postgres:16-alpine
- **Rust:** 1.94.0 (release mode)
- **Tool:** `tools/loadtest/` (custom Rust load tester, 15s per concurrency level)

## Commits
- **Baseline:** `7796190` (post-code-optimization, from previous benchmark)
- **Pool tuning:** `bc5261d` — PG max_connections 20→50, min_connections 5, Valkey max_size 50
- **Batch persistence:** `15765d4` — single TX for all trades in a fill

## What Changed

### 1. PostgreSQL pool (`crates/shared/src/db.rs`)
| Setting | Before | After |
|---------|--------|-------|
| max_connections | 20 | 50 |
| min_connections | — | 5 |
| acquire_timeout | — | 3s |
| idle_timeout | — | 600s |
| max_lifetime | — | 1800s |

### 2. Valkey pool (`crates/shared/src/cache.rs`)
| Setting | Before | After |
|---------|--------|-------|
| max_size | default (10) | 50 |
| wait_timeout | default | 3s |

### 3. Batch trade persistence (`crates/api/src/routes.rs`)
Replaced per-trade sequential loop with `persist_trades()`:
- **Before:** N trades × (1 INSERT + 1 tx begin + 4 balance queries + 1 tx commit + 1 audit INSERT) = N×8 round-trips
- **After:** 1 tx begin + N trade INSERTs + K balance UPDATEs (K = unique user/asset pairs, aggregated) + N audit INSERTs + 1 tx commit = 1 transaction regardless of N

## Load Test: Resting Orders (no matching)

| Concurrency | Baseline ord/s | New ord/s | Δ | Baseline avg_ms | New avg_ms | Δ |
|-------------|----------------|-----------|---|-----------------|------------|---|
| 1 | 68.2 | 69.8 | +2.3% | 14.67 | 14.31 | -2.5% |
| 2 | 97.4 | 107.3 | **+10.2%** | 20.51 | 18.64 | **-9.1%** |
| 4 | 102.5 | 114.3 | **+11.5%** | 37.58 | 34.91 | **-7.1%** |
| 8 | 1.1 | **116.6** | **+∞** | 6619 | **66.08** | **-99%** |

> **Concurrency 8 was previously a near-total stall** (5s lock TTL timeouts firing constantly because the Valkey pool was exhausted). With pool size 50, connections are always available — the lock acquire/release completes fast enough that the 5s TTL is never hit.

## Load Test: Crossing Orders (matching + settlement)

| Concurrency | Baseline ord/s | New ord/s | Δ | Baseline avg_ms | New avg_ms | Δ |
|-------------|----------------|-----------|---|-----------------|------------|---|
| 1 | 49.7 | 55.4 | **+11.5%** | 20.11 | 18.05 | **-10.2%** |
| 2 | 69.8 | 77.4 | **+10.9%** | 28.63 | 25.82 | **-9.8%** |
| 4 | 72.4 | 78.3 | **+8.1%** | 55.11 | 48.87 | **-11.3%** |

> Crossing improvement reflects both pool tuning (faster lock ops) and batch persistence (single DB transaction per order vs N transactions per trade).

## Load Test: Multi-Pair Crossing (BTC-USDT, ETH-USDT, SOL-USDT)

| Concurrency | ord/s | avg_ms | Notes |
|-------------|-------|--------|-------|
| 1 | 58.5 | 17.08 | Clean |
| 2 | 43.4 | 23.04 | High error rate — balance exhaustion across pairs |
| 4 | 43.2 | 43.05 | High error rate — same cause |

> Errors at concurrency 2+ are balance exhaustion: test users (user-1, user-2) are shared across all pairs. When 3 pairs hammer simultaneously, available USDT runs out. This is a load test data issue, not an engine bug. Multi-pair sharding works correctly — each pair has its own independent lock.

## Analysis

### Why pool size fixed the concurrency-8 stall
The distributed lock sequence is: `SET NX EX` (acquire) → work → `DEL` (release). Both ops need a Valkey connection from the pool. With the default pool size of 10 and 8 concurrent requests all competing for connections, threads were blocking waiting for a connection while *holding the per-pair lock* — causing the 5s TTL to fire and the lock to expire mid-operation. With pool size 50, connections are always available; lock hold time drops to ~1ms, and all 8 concurrent requests can run.

### Why batch persistence improved crossing latency ~10%
Each crossing order previously opened 1 DB transaction per trade (for balance settlement). With batch persistence, all trades for a single order fill are committed in one transaction with aggregated balance deltas. Fewer round-trips to PostgreSQL, less transaction overhead.

### Bottleneck now
At concurrency 4+, the **per-pair distributed lock** is still the serialization point — all orders for the same pair queue behind it. The path forward is multi-pair horizontal scaling (already architecturally supported: each pair has its own lock key).

## Test Results
- ✅ Unit tests: 54/54
- ✅ Integration tests: 103/103

## Next Optimization Targets
1. **Multi-pair horizontal scaling** — split load across BTC/ETH/SOL to push past ~115 ord/s ceiling
2. **Lock TTL tuning** — current 5s is conservative; profiling shows ~1-5ms hold time, TTL could drop to 100-500ms to fail faster under true contention
3. **Flamegraph profiling** — identify hidden hotspots in the 14-18ms single-request path
