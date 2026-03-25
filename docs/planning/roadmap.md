# Roadmap

Prove the stateless matching pattern works — correctness first, then performance.

## Phase 1 — Skeleton + Locking ✅ DONE
- [x] Docker Compose (Valkey + PostgreSQL)
- [x] Valkey client + connection pool (`deadpool-redis`)
- [x] PostgreSQL pool (`sqlx`) + migrations
- [x] Order Book Locking (SET NX EX, backoff + jitter, fencing tokens)
- [x] Lock tests: acquire, release, TTL expiry, contention, owner-only release
- [x] PostgreSQL schema: `pairs`, `orders`, `trades`, `balances`, `audit_log`
- [x] Seed: 3 pairs (BTC/ETH/SOL-USDT) + 2 test users with balances

## Phase 2 — Stateless Matching Engine ✅ DONE
- [x] Cache layer: order book load/save (sorted sets + hashes)
- [x] Matching engine: price-time priority (FIFO)
- [x] Limit orders, Market orders, GTC/IOC/FOK, STP (all 4 modes), partial fills
- [x] Stateless cycle: lock → load → match → write → release
- [x] 47 unit tests (pure in-memory, 0.01s)

## Phase 3 — API + Trading UI ✅ DONE
- [x] Axum REST: pairs, orders, portfolio, orderbook, trades, ticker
- [x] WebSocket feeds: real-time orderbook + trades
- [x] Trading UI: pair selector, order book, order form, portfolio, open orders
- [x] Dashboard UI: KPI cards, throughput/latency charts, lock panel, audit log
- [x] Order validation: tick/lot size, min/max qty, balance check
- [x] End-to-end verified from browser

## Phase 4 — Concurrency & Correctness ✅ DONE
- [x] Per-pair lock in create_order, cancel_order, modify_order
- [x] Collision tests (2 buyers/1 ask, 2 sellers/1 bid, concurrent spawns)
- [x] Double-fill prevention, stale-read prevention
- [x] Sequencing tests (interleaved, cascade, overfill prevention)
- [x] Fencing tokens, version counters
- [x] 34 integration tests (cache + DB + lock)

## Phase 5 — Observability & Metrics ✅ DONE
- [x] Instrument matching path: lock_wait_ms, match_ms, cache_write_ms, total_ms
- [x] Valkey metrics: latency samples, lock_wait samples, order/trade counters
- [x] Dashboard API wired to real Valkey metrics (no more placeholders)
- [x] Dashboard JS polls live endpoints (KPI, throughput, latency, locks, audit)
- [x] Audit events: ORDER_CREATED, TRADE_EXECUTED, ORDER_CANCELLED written to DB
- [x] Audit log viewer shows real events
- [x] Latency percentiles: P50/P95/P99 computation + API endpoint
- [x] Metrics integration test

## Phase 6 — Performance & Optimization ✅ DONE
- [x] Criterion benchmarks: single match, 100-level walk, 1000 sequential, multi-pair scaling
- [x] Price-filtered book loading (ZRANGEBYSCORE with bounds) + integration test
- [x] Batched/lazy book loading (LIMIT offset count) + integration test
- [x] Proper HTTP status codes: 400 validation, 404 not found, 409 conflict
- [x] Unused imports cleaned (`cargo fix`)

## Phase 7 — AWS Deployment ✅ DONE
- [x] Gateway Lambda (HTTP/WS server, stateless reads from cache)
- [x] Worker Lambda (individual order processing, async invoked)
- [x] EC2 backend (PostgreSQL + Valkey only, no sme-api binary)
- [x] SAM infrastructure as code (3 nested stacks: network, backend, frontend)
- [x] CloudFront distribution with S3 frontend + API origins
- [x] Docker cross-compilation for ARM64 Lambda containers
- [x] Full deployment automation with `sam build && sam deploy`

> All phases complete. 85 tests passing. Pattern proven and deployed to AWS.

---

## Removed / Deferred

| Item | Reason |
|------|--------|
| **Stream-based service flow** | Inline matching is the proven pattern; crates kept as reference |
| **Dead letter handling** | Lambda retries work for now; proper DLQ is post-MVP |
| **Stop/Trailing/OCO/Iceberg orders** | Tier 2-3, post-deployment |
| **Circuit breaker / kill switch / rate limiting** | Production safety, post-MVP |

---

## Test Summary

| Suite | Count | Time | Command |
|-------|-------|------|---------|
| Unit (matching logic) | 47 | 0.01s | `cargo test` |
| Integration (cache + DB + lock + metrics + collision) | 38 | ~7s | `cargo test --features integration -- --test-threads=1` |
| **Total** | **85** | **~7s** | |
