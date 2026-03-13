# Serverless Matching Engine — Proof of Concept

> Can an order matching engine run statelessly, relying on distributed cache locking?

A high-performance, stateless matching engine built in **Rust**. Each invocation acquires a per-pair lock, loads the order book from cache, matches, writes results, and releases. If this pattern handles millions of operations without data corruption or race conditions, it can deploy anywhere: Lambda, containers, bare metal.

## Stack

| Layer | Technology |
|-------|-----------|
| Language | **Rust** (tokio async runtime) |
| Cache + Locking + Queues | **Dragonfly** (Redis-compatible, multi-threaded) |
| Database | **PostgreSQL** |
| API | **axum** (HTTP + WebSocket) |
| Frontend | Vanilla HTML/JS (dark theme) |
| Local env | **Docker Compose** |

## Structure

```
.
├── crates/
│   ├── shared/                  # Types, locking, cache, streams, DB
│   ├── matching-engine/         # Core: stateless order matching
│   ├── order-service/           # Order lifecycle
│   ├── transaction-service/     # Trade persistence
│   └── api/                     # REST + WebSocket server (axum)
├── web/
│   ├── trading/                 # Trading UI (pairs, portfolio, order entry)
│   └── dashboard/               # Admin metrics dashboard
├── tests/                       # Integration & load tests
├── docs/                        # Specs, ADRs, design notes
├── docker-compose.yml           # Dragonfly + PostgreSQL
└── Cargo.toml                   # Workspace manifest
```

## Quick Start

```bash
docker compose up -d                    # Start Dragonfly + PostgreSQL
cargo build                             # Build all crates
cargo run --bin sme-api                 # Start API server
# http://localhost:3000/trading/        # Trading UI
# http://localhost:3000/dashboard/      # Metrics dashboard
```

## Features

→ [`docs/specs/features.md`](docs/specs/features.md) — complete feature list by tier

## Roadmap

→ [`docs/planning/roadmap.md`](docs/planning/roadmap.md)

## Decisions

→ [`docs/decisions/`](docs/decisions/)
