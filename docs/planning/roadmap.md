# Roadmap

Prove the stateless matching pattern works — correctness first, then performance, then deployment.

## Phase 1 — Skeleton + Locking ✅
- [x] Docker Compose up (Dragonfly + PostgreSQL)
- [x] `crates/shared`: Dragonfly client + connection pool (`deadpool-redis`)
- [x] `crates/shared`: PostgreSQL pool (`sqlx`) + migrations
- [x] `crates/shared/lock.rs`: Order Book Locking (SET NX EX, backoff + jitter)
- [x] Lock unit tests: acquire, release, TTL expiry, contention
- [x] Lock integration test: multiple tokio tasks competing for same pair
- [x] PostgreSQL schema: `pairs`, `orders`, `trades`, `balances`, `audit_log`
- [x] Seed script: create test pairs (BTC-USDT, ETH-USDT, SOL-USDT) + test users
- [x] `cargo build` + `cargo test` green

## Phase 2 — Stateless Matching Engine ✅
- [x] `crates/shared/cache.rs`: order book load/save (sorted sets + hashes)
- [x] `crates/shared/engine.rs`: matching logic
  - [x] Limit orders (price-time priority)
  - [x] Market orders (walk the book)
  - [x] TIF: GTC (rest), IOC (cancel remainder), FOK (all-or-nothing)
  - [x] Self-Trade Prevention (None, CancelTaker, CancelMaker, CancelBoth)
  - [x] Partial fills
- [x] Stateless cycle: lock → load → match → write → release
- [x] Unit tests: 47 tests covering all order type + TIF + STP combinations
- [x] Integration test: submit 2 crossing limit orders → verify trade
- [x] Integration test: market order against resting book
- [x] Integration test: FOK with insufficient liquidity → no trades
- [x] Multi-pair test: concurrent matching across different pairs (cross-pair isolation)

## Phase 3 — API + Trading UI ✅
- [x] `crates/api`: axum REST server (pairs, orders, portfolio, orderbook, trades, ticker)
- [x] `crates/api`: WebSocket feeds (real-time orderbook + trades)
- [x] `web/trading`: pair selector, order book display, order entry form
- [x] `web/trading`: portfolio view, open orders with cancel
- [x] Order validation: tick/lot size, price bands, min/max, balance check
- [x] End-to-end: place order via UI → match → see trade in UI

## Phase 4 — Concurrency & Correctness ✅
- [x] Collision tests: 2 buyers race for 1 ask, 2 sellers race for 1 bid (lock serializes)
- [x] Collision tests: 3 concurrent sellers via tokio::spawn (exactly 1 fills)
- [x] Verify: no duplicate fills (double-sell prevention test)
- [x] Verify: no overfill (5 sequential sellers against bid of 5 = exactly 5 traded)
- [x] Verify: ordering guarantees (FIFO within price level — price-time priority tests)
- [x] Edge cases: empty book, exact quantity fill, stale-read prevention
- [x] Interleaved buy/sell sequencing with unmatched remainder resting correctly
- [x] Partial fill cascade with version counter advancing per mutation
- [x] Lock contention: sequential workers, lock TTL expiry recovery
- [x] Fencing token: monotonic fence counter on lock acquisition for stale-write detection
- [x] Critical bug fixes: added per-pair lock to create_order, cancel_order, modify_order
- [x] Critical bug fix: order saved to cache only AFTER matching (was before — phantom liquidity)
- [x] Lock TTL increased 1s → 5s, retries 10 → 20 (DB-heavy matching path)
- [x] WebSocket version check: read-only GET instead of corrupting INCR
- [ ] Audit trail: verify complete event log, deterministic replay
- [ ] Lock contention metrics: wait time, retry count, failure rate (collected + exposed)

## Phase 5 — Streams & Full Service Flow
- [x] `crates/shared/streams.rs`: XADD/XREADGROUP/XACK helpers (implemented)
- [ ] Order Service: receive order → validate → persist → publish to stream
- [ ] Transaction Service: consume trades → persist → update balances
- [ ] End-to-end: submit order → match → trade → balance update (via streams)
- [ ] Dead letter handling for failed messages

## Phase 6 — Dashboard + Observability *(partial)*
- [x] `web/dashboard`: KPI cards (orders/sec, matches/sec)
- [x] `web/dashboard`: lock contention panel
- [x] `web/dashboard`: stream depths panel
- [x] `web/dashboard`: throughput + latency charts (canvas, time series)
- [x] `web/dashboard`: audit event log viewer
- [ ] Metrics collection: write real counters/gauges to Dragonfly from matching path
- [ ] Dashboard wired to live Dragonfly metrics (currently shows placeholder data)

## Phase 7 — Performance
- [ ] Benchmark: orders/sec throughput (single pair, `criterion`)
- [ ] Benchmark: scaling across N pairs (10, 100, 1000)
- [ ] Optimize: lazy loading (batch 10 → 20 → 40)
- [ ] Optimize: price-filtered loading
- [ ] Profile: identify bottlenecks (locking, DB writes, cache reads)
- [ ] Latency percentiles: P50, P95, P99 per match cycle

## Phase 8 — Production Deployment *(later)*
- [ ] Lambda packaging (Rust binary + Lambda runtime)
- [ ] IaC (CDK/SAM)
- [ ] Aurora Serverless v2 + ElastiCache/Dragonfly Cloud
- [ ] Observability, security, runbooks

> Phases 1-4 = DONE. Phase 5-7 = PoC. Phase 8 only after the pattern is proven.

---

## Test Summary

| Suite | Count | Time | Command |
|-------|-------|------|---------|
| Unit tests (pure matching logic) | 47 | 0.01s | `cargo test` |
| Integration tests (cache + DB + lock + collision) | 34 | ~7s | `cargo test --features integration -- --test-threads=1` |
| **Total** | **81** | **~7s** | |
