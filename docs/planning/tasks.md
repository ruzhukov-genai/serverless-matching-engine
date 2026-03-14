# Task Backlog

All PoC tasks complete. ✅

---

## Phase 5 — Observability & Metrics ✅

- [x] **5.1** Instrument matching path with timing (lock_wait_ms, match_ms, cache_write_ms, total_ms)
- [x] **5.2** Write metrics to Dragonfly (metrics.rs: latency, lock_wait, orders, trades)
- [x] **5.3** Wire dashboard API to real Dragonfly metrics
- [x] **5.4** Dashboard JS polls live endpoints (no more placeholders)
- [x] **5.5** Write audit events (ORDER_CREATED, TRADE_EXECUTED, ORDER_CANCELLED)
- [x] **5.6** Audit log viewer verified with real data

## Phase 6 — Performance & Optimization ✅

- [x] **6.1** Criterion benchmarks: single match, 100-level walk, 1000 sequential
- [x] **6.2** Multi-pair scaling benchmarks (10×100, 100×10)
- [x] **6.3** Price-filtered book loading (ZRANGEBYSCORE with bounds) + integration test
- [x] **6.4** Batched/lazy book loading (LIMIT offset count) + integration test
- [x] **6.5** Latency percentiles (P50/P95/P99) in metrics.rs + API endpoint
- [x] **6.6** *(merged into 6.5)*

## Cleanup ✅

- [x] **C.1** Remove unused imports (`cargo fix`)
- [x] **C.3** Proper HTTP status codes (400/404/409 via AppErrorKind)

## Deferred

- **C.2** Clean up Order/Transaction Service crates — kept as reference for stream-based architecture
