# System Architecture

## Overview

This system replaces a stateful, always-on matching engine with serverless Lambda functions triggered by message queues. The core challenge is maintaining correct ordering and consistency without a persistent in-memory order book.

## Components

### Matching Engine (Lambda)
- Triggered by: order match queue
- Responsibilities: load order book, match orders, write results to DB
- Stateless mode: loads and finalizes per-invocation

### Order Service (Lambda)
- Triggered by: single order queue
- Responsibilities: order lifecycle management, stat dispatching
- Sub-components: **StatDispatcher** (scheduled Lambda) and **StatsUpdater** (queue-triggered)

### Transaction Service (Lambda)
- Triggered by: transaction queue
- Responsibilities: process and persist trade transactions

### Redis
- Order Book write-through cache (sorted by price per side)
- Distributed locking (`book:{pair_id}:lock`)
- Message queue per pair (`book:{pair_id}:queue`)
- Version counter (`book:{pair_id}:version`)

### RabbitMQ / SQS
- Message bus between services
- Single consolidated queue (target state)

### Database
- Persistent order & transaction storage
- Async writes (with cache-first reads)

## Data Flow

```
Client Order Request
        │
        ▼
  [Order Service Lambda]
        │
        ├─► Redis: RPUSH book:{pair_id}:queue {message}
        ├─► Acquire lock (SET NX EX)
        └─► [Matching Engine Lambda]
                │
                ├─► Load Order Book (Redis cache → DB fallback)
                ├─► Match Orders
                ├─► Write results to DB (async, version-guarded)
                ├─► INCR book:{pair_id}:version
                └─► Release lock
```

## Scalability Approach

| Phase | Approach |
|-------|----------|
| 1 | Stateless mode (single instance) |
| 2 | Order Book Locking |
| 3 | Single consolidated queue |
| 4 | Lambda concurrency scaling |
| 5 | RedLock for multi-node Redis |

## Open Questions

> See [`docs/brainstorm/`](brainstorm/) for active explorations.

- What DB? (DynamoDB vs Aurora Serverless vs RDS Proxy)
- Cold start latency impact on matching throughput?
- SQS vs RabbitMQ for Lambda triggers?
- Order Book size limits — when does lazy loading break down?
