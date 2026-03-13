# Architecture

## Core Concept

Stateless matching engine workers (Rust async tasks) that:
1. Acquire a per-pair distributed lock (Dragonfly `SET NX EX`)
2. Load the order book slice from cache (sorted sets)
3. Match the incoming order (price-time priority)
4. Write results to cache + DB
5. Release the lock

Workers are interchangeable. Pairs are isolated by lock.

## Technology Stack

| Layer | Technology | Crate |
|-------|-----------|-------|
| Async runtime | tokio (multi-threaded) | `tokio` |
| Dragonfly client | redis-rs + connection pool | `redis`, `deadpool-redis` |
| PostgreSQL | sqlx (compile-time checked queries) | `sqlx` |
| Serialization | serde + MessagePack | `serde`, `rmp-serde` |
| Logging/Tracing | tracing | `tracing`, `tracing-subscriber` |

## Components

### Matching Engine (`crates/matching-engine`)
- Consumes orders from Dragonfly Stream
- Lock → load → match → write → release cycle
- Stateless: no in-memory state between invocations

### Order Service (`crates/order-service`)
- Order lifecycle (create, cancel, update)
- Validates and persists to PostgreSQL
- Publishes to match stream

### Transaction Service (`crates/transaction-service`)
- Consumes trade events
- Persists trades, updates balances

### Shared (`crates/shared`)
- Dragonfly client factory + health check
- Order Book Locking (acquire, release, backoff)
- Stream producer/consumer helpers
- Common types (Order, Trade, Pair)
- PostgreSQL connection + migrations

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
[Matching Engine Worker]     ← tokio task picks it up
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
2. **No lost orders** — every order is matched, resting, or cancelled
3. **Sequential consistency per pair** — processed in submission order
4. **Cross-pair independence** — pairs never block each other
5. **Lock safety** — crashed worker does not permanently block (TTL expiry)
