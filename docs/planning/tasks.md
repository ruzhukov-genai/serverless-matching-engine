# Task Backlog

Granular tasks for remaining PoC work. Each task = one commit.

---

## Phase 5 — Observability & Metrics

### 5.1 Instrument matching path with timing
- [ ] Add `Instant::now()` timing around lock acquire, book load, match, cache write in `create_order`
- [ ] Log structured timing fields: `lock_wait_ms`, `match_ms`, `cache_write_ms`, `total_ms`
- [ ] Add trade count + fill ratio to log output
- **File:** `crates/api/src/routes.rs`
- **Test:** place order, check logs contain timing fields

### 5.2 Write metrics to Dragonfly
- [ ] Create `crates/shared/src/metrics.rs` with helpers:
  - `record_match_latency(pool, pair_id, ms)` → `LPUSH metrics:{pair_id}:latency`
  - `increment_order_count(pool, pair_id)` → `INCR metrics:{pair_id}:orders`
  - `increment_trade_count(pool, pair_id)` → `INCR metrics:{pair_id}:trades`
  - `record_lock_wait(pool, pair_id, ms)` → `LPUSH metrics:{pair_id}:lock_wait`
- [ ] Call metrics helpers from `create_order` handler
- **Test:** integration test — place order, read metrics from Dragonfly

### 5.3 Wire dashboard API to real metrics
- [ ] `GET /api/metrics` → read from `metrics:*` keys instead of DB COUNT
- [ ] `GET /api/metrics/throughput` → aggregate from `metrics:*:orders` per minute
- [ ] `GET /api/metrics/locks` → read from `metrics:*:lock_wait`
- [ ] Add `GET /api/metrics/latency` → percentiles from `metrics:*:latency` lists
- **Files:** `crates/api/src/routes.rs`
- **Test:** curl endpoints, verify non-zero data after placing orders

### 5.4 Dashboard JS: fetch real data
- [ ] Update `web/dashboard/app.js` to poll real `/api/metrics/*` endpoints
- [ ] Remove hardcoded placeholder data
- [ ] KPI cards show live order/trade counts
- [ ] Charts plot real throughput + latency time series
- **Test:** open dashboard after placing orders, verify charts update

### 5.5 Write audit events
- [ ] Insert to `audit_log` on: order created, order cancelled, trade executed
- [ ] Fields: `sequence` (auto-increment), `pair_id`, `event_type`, `payload` (JSON)
- [ ] Event types: `ORDER_CREATED`, `ORDER_CANCELLED`, `ORDER_FILLED`, `TRADE_EXECUTED`
- **File:** `crates/api/src/routes.rs`
- **Test:** integration test — place crossing orders, verify audit_log rows

### 5.6 Wire audit log viewer
- [ ] `GET /api/audit` already reads from `audit_log` table — verify it works with real data
- [ ] Dashboard audit panel: show real events after 5.5 is done
- **Test:** place orders, open dashboard, verify audit entries appear

---

## Phase 6 — Performance & Benchmarks

### 6.1 Add criterion benchmarks
- [ ] Create `crates/shared/benches/matching.rs`
- [ ] Benchmark: single limit order match (1 ask, 1 buy) — baseline
- [ ] Benchmark: market order walking 100-level book
- [ ] Benchmark: 1000 orders matched sequentially (same pair)
- [ ] Add `criterion` to workspace dev-dependencies
- **Test:** `cargo bench` produces results

### 6.2 Benchmark multi-pair scaling
- [ ] Benchmark: 10 pairs, 100 orders each, sequential
- [ ] Benchmark: 100 pairs, 10 orders each, sequential
- [ ] Measure: total time, per-order average, lock contention ratio
- **Test:** `cargo bench` produces comparative results

### 6.3 Price-filtered book loading
- [ ] Modify `cache::load_order_book` to accept optional price filter
- [ ] For buy: only load asks where score <= buy price
- [ ] For sell: only load bids where score >= -sell_price
- [ ] Use `ZRANGEBYSCORE` with score bounds instead of `-inf +inf`
- **File:** `crates/shared/src/cache.rs`
- **Test:** unit test — filtered load returns subset; integration test — filtered match

### 6.4 Lazy book loading
- [ ] Load first N orders (e.g., 10), match what we can
- [ ] If all consumed and incoming has remaining, load next batch (20, then 40)
- [ ] Reduces cache reads for orders that rest without matching
- **File:** `crates/shared/src/cache.rs`, `crates/shared/src/engine.rs`
- **Test:** integration test — large book, verify only needed levels loaded

### 6.5 Profile and identify bottlenecks
- [ ] Run benchmarks from 6.1 with `--profile-time 10`
- [ ] Identify: is bottleneck locking, cache read, matching, or DB write?
- [ ] Document findings in `docs/planning/performance-report.md`
- **Depends on:** 6.1, 6.2

### 6.6 Latency percentiles
- [ ] Calculate P50, P95, P99 from recorded latency data (5.2)
- [ ] Add to dashboard API response
- [ ] Display in dashboard latency chart
- **Depends on:** 5.2, 5.3
- **Test:** verify percentile values in API response

---

## Cleanup / Tech Debt

### C.1 Remove unused imports
- [ ] Run `cargo fix --workspace` to clean all unused import warnings
- **Test:** `cargo build` produces zero warnings

### C.2 Clean up Order/Transaction Service crates
- [ ] Add README noting these are deferred (stream-based flow, not used in PoC)
- [ ] Or: remove the crates entirely if we want a leaner repo
- **Decision needed:** keep as reference or remove?

### C.3 API error responses
- [ ] Return proper HTTP status codes (400 for validation, 404 for not found, 409 for conflict)
- [ ] Currently some errors return 500 or 422 generically
- **File:** `crates/api/src/routes.rs`

---

## Task Dependencies

```
5.1 → 5.2 → 5.3 → 5.4
                5.5 → 5.6
                      6.1 → 6.2 → 6.5
                      6.3 (independent)
                      6.4 (independent)
                      5.2 + 5.3 → 6.6
```
