# ADR-004: Dragonfly for Cache/Queue, PostgreSQL for Persistence

**Status:** Accepted

## Context

The prototype needs:
1. An in-memory store for order book cache, distributed locking, and message queues
2. A persistent database for orders and transactions
3. Minimal infrastructure complexity for local development

Redis was the initial assumption, but alternatives now offer significantly better performance on modern multi-core hardware.

## Decision

### Cache + Queue: Dragonfly

Use **Dragonfly** as a drop-in Redis replacement for all in-memory operations:
- Order Book cache (sorted sets)
- Distributed locking (`SET NX EX`)
- Message queues via **Streams** (`XADD` / `XREADGROUP`)

**Why Dragonfly over Redis:**
- Multi-threaded, shared-nothing architecture — uses all CPU cores
- 10-25x throughput over Redis (10M+ ops/sec vs ~180K)
- 30% less memory for same dataset
- No fork-based snapshotting — no memory spikes during persistence
- Full RESP protocol compatibility — same clients, same commands
- Streams support — our queue design works unchanged

**Why not Garnet:** Newer, incomplete command coverage, .NET dependency
**Why not KeyDB:** Only 2-5x improvement, development pace slowing

### Persistence: PostgreSQL

Use **PostgreSQL** for all persistent storage:
- Order data (with versioning)
- Transaction/trade records
- Pair configuration

**Why PostgreSQL:**
- ACID compliance critical for financial data
- Strong indexing for price-range queries on order books
- Battle-tested in trading systems
- Natural migration path to Aurora Serverless v2 for production
- Rich ecosystem (partitioning by pair_id, read replicas)

## Consequences

**Benefits:**
- 3-service Docker Compose stack (Dragonfly + PostgreSQL + app nodes)
- No RabbitMQ/Kafka dependency — Dragonfly Streams handles queuing
- Redis-compatible API — can swap back to Redis/ElastiCache if needed
- Massive headroom for throughput scaling

**Risks:**
- Dragonfly is younger than Redis — smaller community, fewer battle scars
- No AOF persistence (RDB snapshots only) — acceptable for cache layer
- No Redis modules (RedisJSON, RediSearch) — not needed for this use case

## Migration Path

- **Local dev:** Dragonfly in Docker
- **AWS production:** ElastiCache Redis or Dragonfly Cloud — API-compatible either way
- **DB production:** Aurora PostgreSQL Serverless v2
