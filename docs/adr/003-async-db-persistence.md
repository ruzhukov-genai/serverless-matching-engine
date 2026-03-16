# ADR-003: Async DB Persistence Off Hot Path

**Status:** Accepted  
**Date:** 2026-03-15  
**Decision makers:** Roman Zhukov

## Context

Order processing has a hot path: validate → lock balance → match → respond. Originally, `insert_order_db` and balance lock ran sequentially before matching, adding ~4ms to every order.

## Decision

**Only `lock_balance` runs on the hot path.** Everything else is deferred:

### Hot path (blocks order processing):
1. Validate order (in-memory pairs cache, ~2µs)
2. `lock_balance` — UPDATE balances SET available=available-X, locked=locked+X (~2-3ms)
3. Lua EVAL in Dragonfly — atomic match against order book (~2ms)

### Deferred (fire-and-forget via mpsc channel):
4. `insert_order_db` — INSERT into orders table
5. `persist_trades` — INSERT trades + UPDATE resting orders + balance settlements
6. Audit log entries
7. Cache key updates (orderbook, ticker, etc.)

### Implementation:
- `PersistJob` struct sent via `mpsc::Sender<PersistJob>` (channel size: 1000)
- Background persist worker consumes from channel, uses `pg_bg` pool
- If channel is full, spawns a direct persist task (never drops)
- Cache updates (`update_order_cache_after_processing`) run via `tokio::spawn`

## Consequences

- Hot path: ~5ms median (was ~20ms)
- Orders are "in-flight" between match and DB persist (~10-50ms gap)
- If worker crashes after match but before persist: Dragonfly has the trade, PG doesn't. Acceptable for PoC. Production needs WAL or idempotent replay.
- Two PG pools: `pg` (hot, 60 conns) for balance locks, `pg_bg` (background, 40 conns) for persistence
- `lock_balance` provides the atomicity guarantee — if funds are insufficient, the order is rejected before matching

## Alternatives Considered

- **Synchronous persist:** Simpler, consistent, but 4x slower. Ruled out for throughput.
- **Write-ahead log in Dragonfly:** Persist a job queue in Dragonfly, separate persist service consumes it. Better crash recovery but adds operational complexity.
