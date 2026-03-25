# ADR-005: Per-Pair Queue Consumers with Bounded Concurrency

**Status:** Accepted  
**Date:** 2026-03-15  
**Decision makers:** Roman Zhukov

## Context

Originally, a single BRPOP loop consumed all orders from `queue:orders`. Orders for BTC-USDT blocked ETH-USDT processing. Under burst (50 concurrent orders), queue latency reached 542ms median.

## Decision

**Per-pair queues with bounded concurrent processing.**

### Queue routing:
- Gateway pushes to `queue:orders:{pair_id}` (e.g., `queue:orders:BTC-USDT`)
- Worker spawns one consumer task per pair
- Fallback consumer on `queue:orders` for backward compatibility
- Same pattern for cancellations: `queue:cancellations:{pair_id}`

### Concurrency control:
- **Semaphore(3) per pair** — at most 3 orders process concurrently per pair
- After each BRPOP wake, drain up to 2 more orders with non-blocking RPOP (batch of 3)
- Each order spawns its own tokio task (fire-and-forget with semaphore permit)

### Why semaphore=3, not higher:
- `lock_balance` does UPDATE on `balances` table — same-user orders contend on PG row locks
- Lua EVAL serializes in Valkey — concurrent EVALs queue up
- Benchmarked: sem=10 caused db_pre median to spike 3x and lua to spike 4x
- sem=3 balances throughput (3x vs sequential) with acceptable contention

### Current pairs (hardcoded):
```rust
const PAIRS: &[&str] = &["BTC-USDT", "ETH-USDT", "SOL-USDT"];
```
Future: load dynamically from pairs table.

## Consequences

- Cross-pair orders process in parallel (BTC and ETH don't block each other)
- Burst queue latency: 542ms → 258ms (-52%)
- Sequential throughput: 20.5ms → 5.4ms per order (-74%)
- Adding a new pair requires updating PAIRS const (or making it dynamic)
- Cancellation consumers use sem=5 (lighter operations, less contention)

## Alternatives Considered

- **Unbounded concurrency:** Spawn task per order with no semaphore. Causes PG connection exhaustion and row-lock storms under burst.
- **Batch matching:** Accumulate N orders, match as a batch in one Lua call. Better throughput but much more complex Lua script. Future optimization.
- **Multiple consumer tasks per pair:** 3 BRPOP loops instead of 1 + semaphore. Works but wastes idle connections on low-volume pairs.

---

_Updated 2026-03-25: Dragonfly replaced with Valkey (ElastiCache) for managed serverless cache._
