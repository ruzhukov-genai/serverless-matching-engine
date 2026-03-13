# Serverless Matching Engine — Proof of Concept

> Can an order matching engine run statelessly, relying on distributed cache locking?

A high-performance, stateless matching engine built in **Rust**. Each invocation acquires a per-pair lock, loads the order book from cache, matches, writes results, and releases. If this pattern handles millions of operations without data corruption or race conditions, it can deploy anywhere: Lambda, containers, bare metal.

## Stack

| Layer | Technology |
|-------|-----------|
| Language | **Rust** (tokio async runtime) |
| Cache + Locking + Queues | **Dragonfly** (Redis-compatible, multi-threaded) |
| Database | **PostgreSQL** |
| Local env | **Docker Compose** |

## Structure

```
.
├── docs/                        # Specs, ADRs, design notes
├── crates/
│   ├── matching-engine/         # Core: stateless order matching
│   ├── order-service/           # Order lifecycle
│   ├── transaction-service/     # Trade persistence
│   └── shared/                  # Dragonfly client, locking, streams, types
├── tests/                       # Integration & load tests
├── docker-compose.yml           # Dragonfly + PostgreSQL
├── Cargo.toml                   # Workspace manifest
└── scripts/                     # Dev & test scripts
```

## Quick Start

```bash
docker compose up -d                    # Start Dragonfly + PostgreSQL
cargo build                             # Build all crates
cargo test                              # Run tests
cargo bench                             # Run benchmarks
```

## Roadmap

→ [`docs/planning/roadmap.md`](docs/planning/roadmap.md)

## Decisions

→ [`docs/decisions/`](docs/decisions/)
