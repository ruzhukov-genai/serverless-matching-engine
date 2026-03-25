# Benchmark Report: Code Optimization (2026-03-14)

## Environment
- **Instance:** AWS t3a.large (2 vCPU, 8GB RAM)
- **OS:** Linux 6.17.0-1007-aws (Ubuntu)
- **Rust:** 1.94.0 (release mode)
- **Valkey:** localhost:6379 (single instance)
- **PostgreSQL:** localhost:5432
- **Tool:** `tools/loadtest/` (custom Rust load tester)

## Commits Compared
- **Before:** `f22324a` (pre-optimization)
- **After:** `7796190` (post-optimization)

## What Changed (Optimization Summary)
1. **Eliminated pairs table DB queries** — parse base/quote from pair_id string instead of querying DB on every order (3+ queries removed per order)
2. **Fixed N+1 Redis pattern** — batch HGET via pipeline instead of individual calls per order
3. **Direct Decimal→f64** — `Decimal::to_f64()` instead of `.to_string().parse::<f64>()`
4. **Code dedup** — consolidated 4 duplicate patterns, -39 net lines

## Unit Tests (54 tests, pure in-memory)

| Commit | Run 1 | Run 2 | Run 3 |
|--------|-------|-------|-------|
| Before (`f22324a`) | 0.01s | 0.01s | 0.01s |
| After (`7796190`) | 0.01s | 0.01s | 0.01s |

**No change** — unit tests are CPU-bound matching logic, no I/O. Sub-millisecond.

## Integration Tests (103 tests, Valkey + PostgreSQL)

| Commit | Run 1 | Run 2 | Run 3 | Avg |
|--------|-------|-------|-------|-----|
| Before (`f22324a`) | 10.63s | 11.00s | 9.74s | **10.46s** |
| After (`7796190`) | 9.93s | 11.75s | 9.92s | **10.53s** |

**Within variance** — integration tests are dominated by lock contention, DB setup/teardown, and `--test-threads=1` serialization. The optimization target (DB query elimination, Redis pipelining) only affects the API handler hot path, which integration tests exercise but don't stress.

## Load Test: Resting Orders (no matching)

Orders placed but not matched — tests order insertion + validation + cache write.

| Concurrency | Before (ord/s) | After (ord/s) | Δ | Before avg_ms | After avg_ms | Δ |
|-------------|----------------|---------------|---|---------------|--------------|---|
| 1 | 66.5 | 68.2 | **+2.6%** | 15.04 | 14.67 | **-2.5%** |
| 2 | 97.6 | 97.4 | ~0% | 20.49 | 20.51 | ~0% |
| 4 | 99.7 | 102.5 | **+2.8%** | 39.96 | 37.58 | **-5.9%** |
| 8 | 1.0 | 1.1 | — | 6986 | 6619 | — |

**Note:** At concurrency 8, lock contention dominates (5s TTL, single-pair lock). The improvement at concurrency 1 and 4 reflects fewer DB round-trips per order.

## Load Test: Crossing Orders (matching + settlement)

Every 2nd order crosses — tests the full path: validate → lock → match → settle → audit.

| Concurrency | Before (ord/s) | After (ord/s) | Δ | Before avg_ms | After avg_ms | Δ |
|-------------|----------------|---------------|---|---------------|--------------|---|
| 1 | 49.8 | 49.7 | ~0% | 20.08 | 20.11 | ~0% |
| 2 | 65.7 | 69.8 | **+6.2%** | 30.40 | 28.63 | **-5.8%** |
| 4 | — | 72.4 | — | — | 55.11 | — |

**Crossing at concurrency 2 shows the clearest improvement** — 6.2% more throughput, 5.8% lower latency. This is where the eliminated DB queries and pipelined Redis reads help most: the matching path now does zero pairs-table queries and fetches orders in a single pipeline instead of N individual calls.

## Analysis

### Why the improvement is moderate (~3-6%)
The current **bottleneck is the distributed lock** (Valkey SET NX EX), not the DB or Redis reads. All orders for the same pair serialize through a single lock, so:
- At concurrency 1: lock wait ≈ 0, each order runs ~15ms (dominated by DB insert + cache write)
- At concurrency 2-4: lock contention starts, and the reduced time-under-lock from fewer DB queries translates to ~6% improvement
- At concurrency 8+: lock timeout (5s TTL) causes near-total stall

### Where the real wins are
The DB query elimination pays off in **sustained throughput**: fewer connections, less DB load, better connection pool utilization under pressure. This matters more at scale than in a single-pair benchmark.

### Next optimization targets
1. **Multi-pair sharding** — distribute load across pair-specific locks (already supported)
2. **Connection pooling tuning** — increase PostgreSQL/Valkey pool sizes
3. **Batch DB writes** — combine multiple trade inserts into single multi-row INSERT
4. **Lock-free reads** — serve orderbook/ticker from Valkey without locking
5. **Consider bb8 or mobc** for connection pool alternatives
6. **Profile with flamegraph** to find hidden hotspots

## Code Size Impact

| File | Before (lines) | After (lines) | Δ |
|------|----------------|---------------|---|
| routes.rs | 1267 | 1226 | -41 |
| cache.rs | 264 | 254 | -10 |
| engine.rs | 907 | 905 | -2 |
| metrics.rs | 223 | 215 | -8 |
| types.rs | 140 | 158 | +18 (moved methods here) |
| **Total** | **2801** | **2758** | **-43** |
