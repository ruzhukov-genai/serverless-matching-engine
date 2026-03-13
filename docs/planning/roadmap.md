# Roadmap

The goal is to prove the stateless matching pattern works — correctness first, then performance, then deployment.

## Phase 1 — Skeleton + Locking *(start here)*
- [ ] Docker Compose up (Dragonfly + PostgreSQL)
- [ ] Shared Dragonfly client + connection health check
- [ ] Order Book Locking implementation (SET NX EX, backoff with jitter)
- [ ] Lock correctness test: multiple workers competing for same pair lock
- [ ] PostgreSQL schema: orders, transactions, pairs (minimal)

## Phase 2 — Stateless Matching Engine
- [ ] Order book load from Dragonfly sorted sets
- [ ] Basic matching logic (limit orders: price-time priority)
- [ ] Stateless cycle: lock → load → match → write → release
- [ ] Single-pair integration test: submit orders, verify matches
- [ ] Multi-pair test: concurrent matching across different pairs

## Phase 3 — Concurrency & Correctness
- [ ] Stress test: many workers, same pair, rapid-fire orders
- [ ] Verify no race conditions (duplicate fills, lost orders, version conflicts)
- [ ] Verify ordering guarantees (earlier orders matched first)
- [ ] Edge cases: partial fills, cancel during match, empty book
- [ ] Lock contention metrics: wait time, retry count, failure rate

## Phase 4 — Streams & Service Communication
- [ ] Dragonfly Streams: order submission → matching → transaction flow
- [ ] Consumer groups per service
- [ ] End-to-end flow: submit order → match → trade → balance update
- [ ] Dead letter handling for failed messages

## Phase 5 — Performance
- [ ] Load test: orders/sec throughput (single pair)
- [ ] Load test: throughput scaling across N pairs
- [ ] Lazy loading optimization (batch 10 → 20 → 40)
- [ ] Price-filtered loading (only relevant side of book)
- [ ] Identify bottlenecks: locking, DB writes, cache reads

## Phase 6 — Production Deployment *(later)*
- [ ] Lambda packaging + SQS triggers
- [ ] IaC (CDK/SAM)
- [ ] Aurora Serverless v2 + ElastiCache/Dragonfly Cloud
- [ ] Observability, security, runbooks

> Phases 1-5 are the PoC. Phase 6 happens only after the pattern is proven.
