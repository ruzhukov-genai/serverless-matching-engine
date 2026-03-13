# Serverless Matching Engine — Proof of Concept

> Can an order matching engine run statelessly, relying on distributed cache locking?

This project proves (or disproves) that a matching engine can operate without persistent in-memory state — loading the order book from cache on every invocation, matching under a distributed lock, and writing results back. If this works reliably under concurrency, it can run anywhere: Lambda, containers, bare processes.

## The Core Question

A traditional matching engine holds the order book in memory. That makes it fast but hard to scale horizontally and impossible to run serverlessly. This PoC tests the alternative:

1. **Lock** the order book for a trading pair (Dragonfly/Redis `SET NX EX`)
2. **Load** the relevant slice from cache (sorted sets)
3. **Match** the incoming order
4. **Write** results back to cache + DB
5. **Release** the lock

If multiple workers can do this concurrently across pairs — and sequentially within a pair — without data corruption, race conditions, or unacceptable latency, the pattern is viable.

## Stack

| Component | Technology |
|-----------|-----------|
| Cache + Locking + Queues | **Dragonfly** (Redis-compatible, multi-threaded) |
| Database | **PostgreSQL** |
| Services | **Node.js / TypeScript** |
| Local env | **Docker Compose** |

## Structure

```
.
├── docs/                        # Specs, ADRs, design notes
├── services/
│   ├── matching-engine/         # Core: stateless order matching
│   ├── order-service/           # Order lifecycle
│   └── transaction-service/     # Trade persistence
├── shared/                      # Dragonfly utilities, locking, streams
├── tests/                       # Integration & load tests
├── docker-compose.yml           # Dragonfly + PostgreSQL
└── scripts/                     # Dev & test scripts
```

## Quick Start

```bash
docker compose up -d             # Start Dragonfly + PostgreSQL
# (service implementation TBD)
```

## Roadmap

→ [`docs/planning/roadmap.md`](docs/planning/roadmap.md)
