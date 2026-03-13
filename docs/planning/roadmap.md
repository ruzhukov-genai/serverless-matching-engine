# Roadmap

Prove the stateless matching pattern works — correctness first, then performance, then deployment.

## Phase 1 — Skeleton + Locking *(start here)*
- [ ] Docker Compose up (Dragonfly + PostgreSQL)
- [ ] `crates/shared`: Dragonfly client + connection pool (`deadpool-redis`)
- [ ] `crates/shared`: PostgreSQL pool (`sqlx`) + migrations
- [ ] `crates/shared/lock.rs`: Order Book Locking (SET NX EX, backoff + jitter)
- [ ] Lock unit tests: acquire, release, TTL expiry, contention
- [ ] Lock integration test: multiple tokio tasks competing for same pair
- [ ] PostgreSQL schema: `orders`, `transactions`, `pairs` (minimal)
- [ ] `cargo build` + `cargo test` green

## Phase 2 — Stateless Matching Engine
- [ ] `crates/shared/cache.rs`: order book load/save (sorted sets)
- [ ] `crates/matching-engine/engine.rs`: matching logic (price-time priority)
- [ ] Stateless cycle: lock → load → match → write → release
- [ ] Unit tests: matching logic (various order combinations)
- [ ] Integration test: submit 2 crossing orders → verify trade
- [ ] Integration test: partial fill → remainder rests in book
- [ ] Multi-pair test: concurrent matching across different pairs

## Phase 3 — Concurrency & Correctness
- [ ] Stress test: N tokio tasks, same pair, rapid-fire orders
- [ ] Verify: no duplicate fills, no lost orders, no version conflicts
- [ ] Verify: ordering guarantees (FIFO within price level)
- [ ] Edge cases: cancel during match, empty book, exact quantity fill
- [ ] Lock contention metrics: wait time, retry count, failure rate
- [ ] Worker crash simulation: kill task mid-match → verify lock TTL recovery

## Phase 4 — Streams & End-to-End Flow
- [ ] `crates/shared/streams.rs`: XADD/XREADGROUP/XACK helpers
- [ ] Order Service: receive order → persist → publish to stream
- [ ] Transaction Service: consume trades → persist
- [ ] End-to-end: submit order → match → trade → balance update
- [ ] Dead letter handling for failed messages

## Phase 5 — Performance
- [ ] Benchmark: orders/sec throughput (single pair, `criterion`)
- [ ] Benchmark: scaling across N pairs (10, 100, 1000)
- [ ] Optimize: lazy loading (batch 10 → 20 → 40)
- [ ] Optimize: price-filtered loading
- [ ] Profile: identify bottlenecks (locking, DB writes, cache reads)
- [ ] Latency percentiles: P50, P95, P99 per match cycle

## Phase 6 — Production Deployment *(later)*
- [ ] Lambda packaging (Rust binary + Lambda runtime)
- [ ] IaC (CDK/SAM)
- [ ] Aurora Serverless v2 + ElastiCache/Dragonfly Cloud
- [ ] Observability, security, runbooks

> Phases 1-5 = PoC. Phase 6 only after the pattern is proven.
