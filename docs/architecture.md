# Architecture

## Core Concept

Stateless matching engine workers that:
1. Acquire a per-pair distributed lock
2. Load the order book slice from cache
3. Match the incoming order
4. Write results to cache + DB
5. Release the lock

Workers are interchangeable — any worker can process any pair. Pairs are isolated by lock.

## Components

### Matching Engine
- Consumes orders from Dragonfly Stream (`stream:orders:match`)
- Acquires lock for the pair → loads book → matches → writes → releases
- Stateless: no in-memory state between invocations

### Order Service
- Receives order requests (create, cancel, update)
- Validates and persists to PostgreSQL
- Publishes to `stream:orders:match`

### Transaction Service
- Consumes trade events from `stream:transactions`
- Persists trades, updates balances

### Dragonfly (Redis-compatible)
- **Locking:** `book:{pair_id}:lock` (SET NX EX)
- **Order Book Cache:** `book:{pair_id}:bids` / `book:{pair_id}:asks` (sorted sets by price)
- **Versioning:** `book:{pair_id}:version` (INCR)
- **Queues:** Streams with consumer groups

### PostgreSQL
- Source of truth for orders, transactions, balances
- Cache-first reads, async version-guarded writes

## Data Flow

```
Order Request
    │
    ▼
[Order Service] ──► PostgreSQL (persist order)
    │
    ▼
XADD stream:orders:match
    │
    ▼
[Matching Engine Worker]     ← any available worker picks it up
    │
    ├─ SET book:{pair_id}:lock NX EX 1
    ├─ ZRANGEBYSCORE book:{pair_id}:asks (load relevant orders)
    ├─ Match logic (price-time priority)
    ├─ ZADD/ZREM (update book cache)
    ├─ INCR book:{pair_id}:version
    ├─ XADD stream:transactions {trade}
    └─ DEL book:{pair_id}:lock
            │
            ▼
    [Transaction Service] ──► PostgreSQL (persist trade)
```

## Key Invariants to Prove

1. **No duplicate fills** — an order is never matched twice
2. **No lost orders** — every submitted order is either matched, resting, or explicitly cancelled
3. **Sequential consistency per pair** — orders within a pair are processed in submission order
4. **Cross-pair independence** — pairs never block each other
5. **Lock safety** — a crashed worker does not permanently block a pair (TTL expiry)
