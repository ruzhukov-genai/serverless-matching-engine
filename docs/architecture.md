# System Architecture

## Overview

This system replaces a stateful, always-on matching engine with serverless functions triggered by message streams. The core challenge is maintaining correct ordering and consistency without a persistent in-memory order book.

## Technology Stack

| Layer | Technology | Purpose |
|-------|-----------|---------|
| Cache + Locking | **Dragonfly** | Order book cache, distributed locks, message queues |
| Message Queue | **Dragonfly Streams** | Inter-service communication (XADD/XREADGROUP) |
| Database | **PostgreSQL** | Persistent order, transaction, and pair storage |
| Services | **Node.js / TypeScript** | Matching Engine, Order Service, Transaction Service |
| Containerization | **Docker Compose** | Local development environment |

> **Why Dragonfly?** Drop-in Redis replacement with 10-25x throughput, multi-threaded architecture, 30% less memory. See [ADR-004](decisions/ADR-004-dragonfly-postgresql.md).

## Components

### Matching Engine
- Triggered by: order match stream (`stream:orders:match`)
- Responsibilities: load order book, match orders, write results to DB
- Stateless mode: loads and finalizes per-invocation

### Order Service
- Triggered by: order stream (`stream:orders`)
- Responsibilities: order lifecycle management, stat dispatching
- Sub-components: **StatDispatcher** (scheduled) and **StatsUpdater** (stream-triggered)

### Transaction Service
- Triggered by: transaction stream (`stream:transactions`)
- Responsibilities: process and persist trade transactions

### Dragonfly (Redis-compatible)
- Order Book write-through cache (sorted by price per side)
- Distributed locking (`book:{pair_id}:lock`)
- Message streams per service (`stream:*`)
- Version counter (`book:{pair_id}:version`)

### PostgreSQL
- Persistent order & transaction storage
- Pair configuration
- Async writes (with cache-first reads, version-guarded)

## Data Flow

```
Client Order Request
        │
        ▼
  [Order Service]
        │
        ├─► Dragonfly: XADD stream:orders:match {order}
        └─► PostgreSQL: persist order record
                │
                ▼
        [Matching Engine]  ◄── XREADGROUP stream:orders:match
                │
                ├─► Dragonfly: load order book (sorted sets)
                ├─► Acquire lock (SET NX EX)
                ├─► Match orders
                ├─► XADD stream:transactions {trade}
                ├─► Update cache + async DB write (version-guarded)
                └─► Release lock
                        │
                        ▼
                [Transaction Service]  ◄── XREADGROUP stream:transactions
                        │
                        ├─► PostgreSQL: persist transaction
                        └─► Update balances
```

## Dragonfly Key Schemas

```
book:{pair_id}:queue    → List     (pending messages)
book:{pair_id}:lock     → String   (NX EX mutex)
book:{pair_id}:version  → Counter
book:{pair_id}:bids     → ZSet     (score = price)
book:{pair_id}:asks     → ZSet     (score = price)
order:{order_id}        → Hash     (order data cache)

stream:orders           → Stream   (order lifecycle events)
stream:orders:match     → Stream   (orders to match)
stream:transactions     → Stream   (trade events)
stream:update_stats     → Stream   (stat update triggers)
```

## Scalability Path

| Phase | Approach |
|-------|----------|
| 0 | Local Docker Compose (Dragonfly + PostgreSQL + app nodes) |
| 1 | Stateless mode (single instance per service) |
| 2 | Order Book Locking (distributed) |
| 3 | Single consolidated streams |
| 4 | Lambda deployment (SQS triggers) |
| 5 | RedLock for multi-node cache |

## Open Questions

> See [`docs/brainstorm/`](brainstorm/) for active explorations.

- Cold start latency impact on matching throughput?
- Order Book size limits — when does lazy loading break down?
- Dragonfly Streams consumer group behavior under high contention?
