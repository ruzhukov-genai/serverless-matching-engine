# Roadmap

Prove the stateless matching pattern works — correctness first, then performance, then deployment.

## Phase 1 — Skeleton + Locking *(start here)*
- [ ] Docker Compose up (Dragonfly + PostgreSQL)
- [ ] `crates/shared`: Dragonfly client + connection pool (`deadpool-redis`)
- [ ] `crates/shared`: PostgreSQL pool (`sqlx`) + migrations
- [ ] `crates/shared/lock.rs`: Order Book Locking (SET NX EX, backoff + jitter)
- [ ] Lock unit tests: acquire, release, TTL expiry, contention
- [ ] Lock integration test: multiple tokio tasks competing for same pair
- [ ] PostgreSQL schema: `pairs`, `orders`, `transactions`, `balances`
- [ ] Seed script: create test pairs (BTC/USDT, ETH/USDT, etc.)
- [ ] `cargo build` + `cargo test` green

## Phase 2 — Stateless Matching Engine
- [ ] `crates/shared/cache.rs`: order book load/save (sorted sets)
- [ ] `crates/matching-engine/engine.rs`: matching logic
  - [ ] Limit orders (price-time priority)
  - [ ] Market orders (walk the book)
  - [ ] TIF: GTC (rest), IOC (cancel remainder), FOK (all-or-nothing)
  - [ ] Self-Trade Prevention (all modes)
  - [ ] Partial fills
- [ ] Stateless cycle: lock → load → match → write → release
- [ ] Unit tests: matching logic (all order type + TIF combinations)
- [ ] Integration test: submit 2 crossing limit orders → verify trade
- [ ] Integration test: market order against resting book
- [ ] Integration test: FOK with insufficient liquidity → no trades
- [ ] Multi-pair test: concurrent matching across different pairs

## Phase 3 — API + Trading UI
- [ ] `crates/api`: axum REST server (pairs, orders, portfolio, orderbook)
- [ ] `crates/api`: WebSocket feeds (real-time orderbook + trades)
- [ ] `web/trading`: pair selector, order book display, order entry form
- [ ] `web/trading`: portfolio view, open orders with cancel
- [ ] Order validation: tick/lot size, price bands, min/max, balance check
- [ ] End-to-end: place order via UI → match → see trade in UI

## Phase 4 — Concurrency & Correctness
- [ ] Stress test: N tokio tasks, same pair, rapid-fire orders
- [ ] Verify: no duplicate fills, no lost orders, no version conflicts
- [ ] Verify: ordering guarantees (FIFO within price level)
- [ ] Edge cases: cancel during match, empty book, exact quantity fill
- [ ] Lock contention metrics: wait time, retry count, failure rate
- [ ] Worker crash simulation: kill task mid-match → verify lock TTL recovery
- [ ] Audit trail: verify complete event log, deterministic replay

## Phase 5 — Streams & Full Service Flow
- [ ] `crates/shared/streams.rs`: XADD/XREADGROUP/XACK helpers
- [ ] Order Service: receive order → validate → persist → publish to stream
- [ ] Transaction Service: consume trades → persist → update balances
- [ ] End-to-end: submit order → match → trade → balance update
- [ ] Dead letter handling for failed messages

## Phase 6 — Dashboard + Observability
- [ ] `web/dashboard`: KPI cards (orders/sec, matches/sec, latency)
- [ ] `web/dashboard`: lock contention panel, stream depths
- [ ] `web/dashboard`: throughput + latency charts (time series)
- [ ] `web/dashboard`: audit event log viewer
- [ ] Metrics collection: write counters/gauges to Dragonfly from all services

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

> Phases 1-7 = PoC. Phase 8 only after the pattern is proven.
