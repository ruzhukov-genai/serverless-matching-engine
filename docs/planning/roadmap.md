# Roadmap

Prove the stateless matching pattern works — correctness first, then performance.

## Phase 1 — Skeleton + Locking ✅ DONE
- [x] Docker Compose (Dragonfly + PostgreSQL)
- [x] Dragonfly client + connection pool (`deadpool-redis`)
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
- [x] 34 integration tests (cache + DB + lock, ~7s)

## Phase 5 — Observability & Metrics
- [x] Dashboard UI panels (KPI, throughput, latency, lock, streams, audit)
- [ ] Instrument matching path: record latency, lock wait, match count per request
- [ ] Write metrics to Dragonfly (counters/gauges) from API handlers
- [ ] Wire dashboard to real Dragonfly metrics (replace placeholder data)
- [ ] Audit log: write events on order create/cancel/match/trade
- [ ] Audit log viewer: show real events from `audit_log` table

## Phase 6 — Performance & Benchmarks
- [ ] `criterion` benchmark: orders/sec throughput (single pair)
- [ ] `criterion` benchmark: scaling across N pairs (10, 100)
- [ ] Optimize: price-filtered book loading (only load price levels that can match)
- [ ] Optimize: lazy loading (batch 10 → 20 → 40 resting orders)
- [ ] Profile: identify bottlenecks (locking vs DB vs cache)
- [ ] Latency percentiles: P50, P95, P99 per match cycle

> PoC is complete when Phase 6 benchmarks prove the pattern handles target throughput.
> Phases 1-4 = DONE (81 tests). Phase 5-6 = remaining PoC work.

---

## Removed / Deferred

| Item | Reason |
|------|--------|
| **Phase 8 — Production Deployment** | Removed. Lambda/IaC/Aurora is out of PoC scope. |
| **Stream-based service flow** | Order Service and Transaction Service crates exist but the inline matching in the API handler is the proven pattern. Stream decoupling is a production concern, not PoC. |
| **Dead letter handling** | Production concern. |
| **Deterministic replay from audit log** | Deferred until audit events are written (Phase 5). |
| **Stop/Trailing/OCO/Iceberg orders** | Tier 2-3 features, post-PoC. |
| **Circuit breaker / kill switch** | Production safety, not PoC. |
| **Rate limiting** | Production concern. |

---

## Test Summary

| Suite | Count | Time | Command |
|-------|-------|------|---------|
| Unit (matching logic) | 47 | 0.01s | `cargo test` |
| Integration (cache + DB + lock + collision) | 34 | ~7s | `cargo test --features integration -- --test-threads=1` |
| **Total** | **81** | **~7s** | |
