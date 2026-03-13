# CLAUDE.md — Agentic Coding Rules

This file is read by Claude Code, Cursor, Copilot, and other AI coding agents at session start.
It defines project conventions, architecture, and workflow rules.

## Project

**Serverless Matching Engine** — a stateless order matching engine in Rust.
Dragonfly (Redis-compatible) for distributed locking/caching, PostgreSQL for persistence.

## Architecture

```
crates/
  shared/       → types, config, cache (sorted sets), lock (SET NX EX), DB, engine, streams
  api/          → axum REST + WebSocket server (port 3001)
  matching-engine/  → re-exports shared engine (standalone binary, unused in PoC)
  order-service/    → stream consumer (reference impl, unused in PoC)
  transaction-service/ → stream consumer (reference impl, unused in PoC)
web/
  trading/      → vanilla HTML/CSS/JS trading UI
  dashboard/    → vanilla HTML/CSS/JS admin dashboard
```

**Core flow (inline in API handler):**
`POST /api/orders` → validate → insert DB → lock balance → **acquire per-pair lock** → load book → match → update cache → **release lock** → persist trades/updates to DB

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
- **Bug workflow:** find bug → write failing test first → fix → see test pass
- All tests must pass before commit: `cargo test && cargo test --features integration -- --test-threads=1`

### Concurrency
- **Every operation that reads or writes the order book cache MUST hold the per-pair lock**
- Lock key: `book:{pair_id}:lock` (SET NX EX, 5s TTL)
- Fence token: `book:{pair_id}:fence` (monotonic INCR on acquisition)
- Version counter: `version:{pair_id}` (INCR after book mutation)
- Cancel and modify operations re-check order status inside the lock (double-check pattern)
- Incoming orders are saved to cache only AFTER matching, only if they rest (GTC with remaining qty)

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

# Run API
DATABASE_URL=postgres://sme:sme_dev@localhost:5432/matching_engine \
DRAGONFLY_URL=redis://localhost:6379 \
PORT=3001 \
cargo run --bin sme-api
```

### Key URLs (dev)
- Trading UI: `http://localhost:3001/trading/`
- Dashboard: `http://localhost:3001/dashboard/`
- API: `http://localhost:3001/api/pairs`

## File Map

| Path | Purpose |
|------|---------|
| `crates/shared/src/engine.rs` | Matching algorithm + 47 unit tests |
| `crates/shared/src/lock.rs` | Distributed locking (SET NX EX + fencing) |
| `crates/shared/src/cache.rs` | Order book cache (sorted sets + hashes) |
| `crates/shared/src/integration_tests.rs` | 34 integration tests (cache + DB + lock + collision) |
| `crates/api/src/routes.rs` | All API handlers (orders, matching, portfolio) |
| `docs/planning/roadmap.md` | Phase status + what's done/remaining |
| `docs/planning/tasks.md` | Granular task backlog (one task = one commit) |
| `docs/specs/features.md` | Feature spec (Tier 1/2/3) |
| `docs/decisions/ADR-*.md` | Architecture Decision Records |

## Don'ts

- Don't use `f64` for prices or quantities
- Don't use `sqlx::query!` macro
- Don't add external JS/CSS libraries to the frontend
- Don't touch the order book cache without holding the per-pair lock
- Don't run destructive DB operations without asking
- Don't commit code that doesn't compile or pass tests
