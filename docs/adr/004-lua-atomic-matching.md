# ADR-004: Atomic Lua Matching in Valkey

> **Previously:** "Atomic Lua Matching in Dragonfly"

**Status:** Accepted  
**Date:** 2026-03-15  
**Decision makers:** Roman Zhukov

## Context

The matching engine runs stateless (ADR-001). Multiple workers may process orders for the same pair concurrently. The order book lives in Valkey sorted sets. We need atomic read-modify-write of the book to prevent double-fills.

## Decision

**Matching runs as a single Lua EVAL in Valkey.** One script atomically:

1. `ZRANGEBYSCORE` — fetch resting order IDs from opposite book (price-bounded, LIMIT 50)
2. `HMGET` each resting order's fields (price, remaining, user_id, STP mode)
3. Price-time priority matching loop with fill calculations
4. `HSET` updated remaining quantities on partially filled resting orders
5. `ZREM` + `DEL` fully filled resting orders
6. `ZADD` + `HSET` incoming order if it rests (GTC with remainder)
7. `INCR` version counter

### Key design choices:
- **Single round-trip:** ZRANGEBYSCORE runs inside Lua (requires `--default_lua_flags=allow-undeclared-keys` in local Valkey/Redis since order hash keys aren't pre-declared)
- **Price-bounded fetch:** Buy orders only fetch asks ≤ bid price; Sell orders only fetch bids ≥ ask price. Market orders fetch all.
- **LIMIT 50:** Matching more than 50 price levels in one order is rare. Caps Lua execution time.
- **Return format:** Array of [OK, remaining_i, status, trade_count, ...trade_fields]. Parsed in Rust.

### What Lua does NOT do:
- UUID generation (done in Rust after EVAL returns)
- DB persistence (deferred, ADR-003)
- Balance locking (done pre-match in PG, ADR-003)
- FOK orders (use OCC/CAS path in Rust — EVAL can't do speculative matching)

## Consequences

- Atomic matching: no double-fills, no locks, no retries (except FOK)
- Valkey's Lua executor is single-threaded — concurrent EVALs serialize. Under burst, this adds ~13ms p95 to lua_us. Acceptable tradeoff vs lock-based approaches.
- Requires `--default_lua_flags=allow-undeclared-keys` in local dev config (docker-compose.yml)
- Script must be loaded on every connection (no EVALSHA pre-loading currently)
- Price stored as `price_i = price * 10^8` (i64) to avoid Lua floating-point errors

## Alternatives Considered

- **OCC/CAS for all orders:** Load book → match in Rust → CAS write. Works but has retry storms under contention. Used only for FOK now.
- **Distributed lock (SET NX EX):** Per-pair lock before matching. Simpler but adds 1 RT + lock hold time. Abandoned in favor of atomic Lua.
- **In-memory matching with Valkey persistence:** Faster but breaks stateless constraint (ADR-001).

---

_Updated 2026-03-25: Dragonfly replaced with Valkey (ElastiCache) for managed serverless cache._
