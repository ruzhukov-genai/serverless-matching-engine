# Shared: Dragonfly / Redis Utilities

Common Dragonfly (Redis-compatible) utilities shared across all services.

> Uses standard Redis clients (ioredis, node-redis). Dragonfly is a drop-in replacement — same RESP protocol.

## Modules (planned)

| Module | Description |
|--------|-------------|
| `client.ts` | Dragonfly connection factory + health check |
| `lock.ts` | Order Book Locking (acquire, release, backoff) |
| `orderBook.ts` | Sorted set helpers (bids/asks cache) |
| `streams.ts` | Stream producer/consumer helpers (XADD, XREADGROUP, XACK) |
| `version.ts` | INCR / version-guard helpers |

## Key Schemas

```
book:{pair_id}:queue    → List     (pending messages)
book:{pair_id}:lock     → String   (NX EX mutex)
book:{pair_id}:version  → Counter
book:{pair_id}:bids     → ZSet     (score = price)
book:{pair_id}:asks     → ZSet     (score = price)
order:{order_id}        → Hash     (order data cache)
```

## Stream Schemas

```
stream:orders           → order lifecycle events
stream:orders:match     → orders pending matching
stream:transactions     → trade/fill events
stream:update_stats     → stat update triggers (per pair)
```

## Connection

```typescript
// Environment variable
// DRAGONFLY_URL=redis://dragonfly:6379 (Docker Compose)
// DRAGONFLY_URL=redis://localhost:6379 (local)
```
