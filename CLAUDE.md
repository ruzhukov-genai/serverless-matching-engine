# CLAUDE.md — Agentic Coding Rules

This file is read by Claude Code, Cursor, Copilot, and other AI coding agents at session start.
It defines project conventions, architecture, and workflow rules.

## Project

**Serverless Matching Engine** — a stateless order matching engine in Rust.
Dragonfly (Redis-compatible) for distributed locking/caching, PostgreSQL for persistence.

## Architecture

```
crates/
  gateway/      → stateless HTTP/WS gateway (port 3001), reads Dragonfly cache
  worker-lambda/ → Worker Lambda (processes individual orders)
  api/          → local dev worker (BRPOP queue consumer), matching, PG writes
  shared/       → types, config, cache (sorted sets + Lua matching), DB, engine
  matching-engine/  → re-exports shared engine (standalone binary, unused in PoC)
  order-service/    → stream consumer (reference impl, unused in PoC)
  transaction-service/ → stream consumer (reference impl, unused in PoC)
infra/
  template.yaml → Root SAM template with nested stacks
  stacks/       → Network, backend, frontend CloudFormation stacks
  Dockerfile.*  → Lambda container builds (ARM64 cross-compile)
web/
  trading/      → vanilla HTML/CSS/JS trading UI
  dashboard/    → vanilla HTML/CSS/JS admin dashboard
docs/
  adr/          → Architecture Decision Records (READ THESE FIRST)
```

**Core flow (gateway → worker Lambda):**
`POST /api/orders` → gateway validates → async invoke Worker Lambda → 202 Accepted
→ Worker Lambda → lock balance (PG) → Lua EVAL match (Dragonfly) → persist trades (PG) → update cache

**Legacy flow (local dev):**
`POST /api/orders` → gateway validates → `LPUSH queue:orders:{pair_id}` → 202 Accepted
→ worker BRPOP → lock balance (PG) → Lua EVAL match (Dragonfly) → async persist (PG) → update cache

**Key design constraints (see ADRs):**
- Workers MUST be stateless — future Lambda deployment (ADR-001)
- Only dictionary-style rarely-changing data may be cached in worker RAM
- Dragonfly is the primary cache layer; PG is persistence
- Gateway uses shared cache broadcasts for reads (ADR-002)
- DB persistence is off the hot path (ADR-003)
- Matching is atomic Lua in Dragonfly (ADR-004)
- Per-pair queues with bounded concurrency, sem=3 (ADR-005)

## Stack

- **Language:** Rust 2024 edition, toolchain 1.94+
- **Runtime:** tokio 1 (async)
- **HTTP:** axum
- **Cache:** Dragonfly via `deadpool-redis` / `redis` crate
- **DB:** PostgreSQL via `sqlx` (runtime, NOT compile-time macros)
- **Decimals:** `rust_decimal::Decimal` for ALL monetary values — never `f64`
- **Frontend:** vanilla HTML/CSS/JS — NO external CDN/library dependencies

## Coding Rules

### Rust
- Use `sqlx::query()` with `.bind()` — never `sqlx::query!` macro (avoids compile-time DB requirement)
- Enums serialize as strings in JSON (e.g. `"Buy"`, `"Limit"`, `"GTC"`)
- All monetary math uses `Decimal` — no floating point
- Error handling: `anyhow::Result` for application code, `thiserror` for library errors
- Structured logging via `tracing` crate

### Testing
- **Unit tests:** pure in-memory, zero I/O, sub-millisecond — in `engine.rs` test module
- **Integration tests:** behind `--features integration` flag, require Docker services running
- Run integration tests with `--test-threads=1` (shared Dragonfly/PG state)
- **Smoke tests:** `tests/smoke/smoke_test.py` — 10 end-to-end tests (Python + Playwright)
  - T01-T08: API tests (pairs, limit/market orders, matching, ticker, portfolio, orderbook, pair isolation)
  - T09-T10: Browser tests via headless Chromium (orderbook rendering, live trade updates)
  - `--no-browser` flag runs API-only tests (T01-T08) without Playwright
  - Works against both local Docker and AWS Lambda deployments
  - Uses pre-seeded users `user-1` (buyer) and `user-2` (seller)
- **Integration shell tests:** `tests/integration/test_orderbook_pairs.sh` — API + WS pair isolation
- **Bug workflow:** find bug → write failing test first → fix → see test pass
- All tests must pass before commit: `cargo test && cargo test --features integration -- --test-threads=1`

```bash
# Full test suite
cargo test                                                    # unit tests
cargo test --features integration -- --test-threads=1         # integration tests

# Smoke tests (local Docker)
python3 tests/smoke/smoke_test.py

# Smoke tests (AWS)
python3 tests/smoke/smoke_test.py \
  --api https://kpvhsf0ub8.execute-api.us-east-1.amazonaws.com \
  --ws wss://2shnq9yk0c.execute-api.us-east-1.amazonaws.com/ws \
  --frontend https://d3ux5yer0uv7b5.cloudfront.net

# API-only smoke tests (no Playwright dependency)
python3 tests/smoke/smoke_test.py --no-browser
```

### Latency Instrumentation

Every order records three lifecycle timestamps in the `orders` table:
- `received_at` — when Gateway Lambda receives the order from the client
- `matched_at` — when Lua EVAL completes (order matched or placed in book)
- `persisted_at` — when PG transaction commits (set via `NOW()` inside INSERT)

**Latency analysis queries after a benchmark run:**
```sql
-- Average latency breakdown
SELECT
  count(*) as orders,
  avg(extract(epoch from matched_at - received_at) * 1000)::int as avg_match_ms,
  avg(extract(epoch from persisted_at - matched_at) * 1000)::int as avg_persist_ms,
  avg(extract(epoch from persisted_at - received_at) * 1000)::int as avg_total_ms
FROM orders WHERE received_at IS NOT NULL;

-- Percentiles (gateway→match latency)
SELECT
  percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch from matched_at - received_at) * 1000)::int as p50_ms,
  percentile_cont(0.95) WITHIN GROUP (ORDER BY extract(epoch from matched_at - received_at) * 1000)::int as p95_ms,
  percentile_cont(0.99) WITHIN GROUP (ORDER BY extract(epoch from matched_at - received_at) * 1000)::int as p99_ms
FROM orders WHERE received_at IS NOT NULL;
```

### Concurrency
- **Matching is atomic via Lua EVAL** — no distributed lock needed (ADR-004)
- Version counter: `version:{pair_id}` (INCR inside Lua after book mutation)
- Per-pair queue consumers with bounded concurrency: `Semaphore(3)` per pair (ADR-005)
- Cancel and modify operations re-check order status in PG before acting
- Incoming orders rest in cache only after matching (GTC with remaining qty), written inside Lua
- Balance locking: `UPDATE balances SET available=available-X, locked=locked+X WHERE available >= X` (row-level PG lock)

### Frontend
- Dark theme, consistent between Trading UI and Dashboard
- No external dependencies (no React, no CDN, no npm)
- Canvas-based charts (no chart libraries)

## Development Workflow

1. **Read source + logs** before forming any hypothesis about bugs
2. **Reproduce locally** before fixing — run locally, add debug output
3. **Test locally** before declaring done — full end-to-end flow
4. **Commit** with detailed message explaining what + why
5. **One commit per task** — granular, reviewable

### Local Dev

```bash
# Start infrastructure
docker compose up -d

# Build + test
cargo build
cargo test                                                    # unit tests (0.01s)
cargo test --features integration -- --test-threads=1         # integration (needs Docker)

# Run worker (queue consumer, matching, PG writes)
DATABASE_URL=postgres://sme:sme_dev@localhost:5432/matching_engine \
DRAGONFLY_URL=redis://localhost:6379 \
RUST_LOG=info \
./target/release/sme-api

# Run gateway (HTTP/WS, reads Dragonfly, serves UI)
PORT=3001 \
DRAGONFLY_URL=redis://localhost:6379 \
RUST_LOG=info \
./target/release/sme-gateway

# Run benchmark
python3 tools/benchmark.py
```

### Key URLs (dev)
- Trading UI: `http://localhost:3001/trading/`
- Dashboard: `http://localhost:3001/dashboard/`
- API: `http://localhost:3001/api/pairs`
- WebSocket: `ws://localhost:3001/ws/orderbook/BTC-USDT`

## File Map

### Code
| Path | Purpose |
|------|---------|
| `crates/gateway/src/main.rs` | Gateway startup, CacheBroadcasts, AppState |
| `crates/gateway/src/routes.rs` | REST handlers, WS feeds, order submission |
| `crates/api/src/main.rs` | Worker startup, PG pools, seed data |
| `crates/api/src/worker.rs` | Queue consumers, order processing, cache refresh |
| `crates/api/src/routes.rs` | Persist worker, balance lock, validation |
| `crates/shared/src/engine.rs` | Matching algorithm + 47 unit tests |
| `crates/shared/src/cache.rs` | Order book cache (sorted sets + Lua matching) |
| `crates/shared/src/lua/match_order.lua` | Atomic Lua matching script |
| `crates/shared/src/lock.rs` | Distributed locking (SET NX EX + fencing) |
| `crates/shared/src/integration_tests.rs` | 64 integration tests |
| `tests/smoke/smoke_test.py` | 10 end-to-end smoke tests (Python + Playwright) |
| `tests/integration/test_orderbook_pairs.sh` | API + WS pair isolation shell tests |

### Docs (all paths relative to repo root)
| Path | Purpose |
|------|---------|
| `docs/aws-architecture.md` | AWS infra: SAM stacks, Lambda, EC2, gotchas |
| `docs/adr/*.md` | Architecture Decision Records (5 active) |
| `docs/specs/features.md` | Feature spec (Tier 1/2/3 order types, TIF, STP) |
| `docs/specs/matching-engine.md` | Matching engine spec |
| `docs/specs/common-functions.md` | Shared functions spec (locking, cache keys) |
| `docs/planning/roadmap.md` | Phase status + what's done/remaining |
| `docs/planning/tasks.md` | Task backlog |
| `docs/benchmarks/` | Historical benchmark results (2026-03-14) |
| `docs/brainstorm/README.md` | Open questions + resolved decisions |
| `tools/benchmark.py` | Comprehensive load test + server profiling |

### Infrastructure
| Path | Purpose |
|------|---------|
| `infra/template.yaml` | Root SAM template (3 nested stacks) |
| `infra/stacks/network.yaml` | VPC, subnets, security groups |
| `infra/stacks/backend.yaml` | EC2, Lambda ×2, API Gateway, UserData |
| `infra/stacks/frontend.yaml` | S3, CloudFront, OAC |
| `infra/Dockerfile.gateway` | Gateway Lambda Docker build (cross-compile arm64) |
| `infra/Dockerfile.worker` | Worker Lambda Docker build (cross-compile arm64) |

### Symlinks
| Path | Target |
|------|--------|
| `AGENTS.md` | → `CLAUDE.md` |
| `.github/copilot-instructions.md` | → `CLAUDE.md` |

## AWS Deployment

**Infrastructure is SAM nested stacks — NEVER deploy individual stack templates directly.**

- Root stack: `serverless-matching-engine` (deploy via `sam deploy` from repo root)
- Nested: `BackendStack-7JBC7XEVQNKF`, `NetworkStack-1CC1NUX65RXPV`, `FrontendStack-156KD6ROT0G0S`
- Individual `infra/stacks/*.yaml` files are NOT standalone — they reference parent parameters/conditions
- For Lambda code updates without full redeploy: `aws lambda update-function-code`
- Lambda function names: `serverless-matching-engine-gateway`, `serverless-matching-engine-worker`, `serverless-matching-engine-ws-handler`

## Don'ts

- Don't use `f64` for prices or quantities
- Don't use `sqlx::query!` macro
- Don't add external JS/CSS libraries to the frontend
- Don't touch the order book cache without holding the per-pair lock
- Don't run destructive DB operations without asking
- Don't commit code that doesn't compile or pass tests
