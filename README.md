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
│   ├── gateway/                 # HTTP/WS gateway (stateless, reads cache)
│   ├── worker-lambda/           # Worker Lambda (processes individual orders)
│   ├── api/                     # Local dev worker (BRPOP queue consumer)
│   ├── matching-engine/         # Core: stateless order matching
│   ├── order-service/           # Order lifecycle (reference impl)
│   └── transaction-service/     # Trade persistence (reference impl)
├── infra/                       # AWS SAM deployment (Lambda + EC2)
├── web/
│   ├── trading/                 # Trading UI (pairs, portfolio, order entry)
│   └── dashboard/               # Admin metrics dashboard
├── tests/                       # Integration & load tests
├── docs/                        # Specs, ADRs, design notes, architecture
├── docker-compose.yml           # Dragonfly + PostgreSQL
└── Cargo.toml                   # Workspace manifest
```

## Quick Start

### Local Development
```bash
docker compose up -d                    # Start Dragonfly + PostgreSQL
cargo build                             # Build all crates
cargo run --bin sme-api                 # Start worker (queue consumer)
cargo run --bin sme-gateway             # Start gateway (HTTP/WS)
# http://localhost:3001/trading/        # Trading UI
# http://localhost:3001/dashboard/      # Metrics dashboard
```

### AWS Deployment
```bash
cd infra
sam build --use-buildkit
sam deploy --capabilities CAPABILITY_IAM CAPABILITY_NAMED_IAM CAPABILITY_AUTO_EXPAND
```

See [docs/aws-architecture.md](docs/aws-architecture.md) for detailed AWS architecture and deployment guide.

## Features

→ [`docs/specs/features.md`](docs/specs/features.md) — complete feature list by tier

## Roadmap

→ [`docs/planning/roadmap.md`](docs/planning/roadmap.md)

## Decisions

→ [`docs/decisions/`](docs/decisions/)
