# Shared: Redis Utilities

Common Redis utilities shared across all services.

## Modules (planned)

| Module | Description |
|--------|-------------|
| `lock.ts` | Order Book Locking (acquire, release, backoff) |
| `orderBook.ts` | Sorted set helpers (bids/asks cache) |
| `queue.ts` | RPUSH / LPOP wrappers for per-pair message queues |
| `version.ts` | INCR / version-guard helpers |

## Key Schemas

```
book:{pair_id}:queue    → List     (pending messages)
book:{pair_id}:lock     → String   (NX EX mutex)
book:{pair_id}:version  → Counter
book:{pair_id}:bids     → ZSet     (score = price)
book:{pair_id}:asks     → ZSet     (score = price)
order:{order_id}        → Hash     (order data)
```
