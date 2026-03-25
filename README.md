# Serverless Matching Engine

> A high-performance, stateless order matching engine in **Rust**, deployed as AWS Lambda functions.

Each order invocation acquires a per-pair lock, loads the order book from cache, matches atomically via Lua, persists results, and releases. Fully serverless — scales to zero when idle, scales out automatically under load.

## Stack

| Layer | Technology |
|-------|-----------|
| Language | **Rust** (tokio async runtime) |
| Cache + Matching | **Valkey** (ElastiCache, Redis-compatible) — Lua atomic matching |
| Database | **PostgreSQL** (RDS) via RDS Proxy |
| API | **axum** (HTTP + WebSocket) via Lambda Web Adapter |
| Frontend | Vanilla HTML/JS (dark theme) |
| Infrastructure | AWS SAM (Lambda, API Gateway, RDS, ElastiCache, CloudFront) |
| Local dev | **Docker Compose** (Valkey + PostgreSQL) |

## Architecture

```
Client → CloudFront → API Gateway → Gateway Lambda (HTTP/WS)
                                         │
                                    async invoke
                                         │
                                    Worker Lambda
                                    ┌────┴────┐
                                    │ Lua EVAL │ ← Valkey (ElastiCache)
                                    │ (match)  │
                                    └────┬────┘
                                         │
                                    persist trades
                                         │
                                    PostgreSQL (RDS via Proxy)
```

**Core flow:**
`POST /api/orders` → Gateway validates → async invoke Worker Lambda → 202 Accepted
→ Worker: lock balance (PG) → Lua EVAL match (Valkey) → persist trades (PG) → update cache

## Structure

```
crates/
  gateway/        → HTTP/WS gateway Lambda (reads Valkey cache, dispatches orders)
  worker-lambda/  → Worker Lambda (matches orders, persists to PG)
  shared/         → Types, config, cache (sorted sets + Lua matching), DB, engine
  api/            → Local dev worker (BRPOP queue consumer)
infra/
  template.yaml   → Root SAM template (3 nested stacks)
  stacks/         → Network, backend, frontend CloudFormation
  Dockerfile.*    → Lambda container builds
web/
  trading/        → Trading UI (pairs, portfolio, order entry)
  dashboard/      → Admin metrics dashboard
migrations/       → Timestamped SQL migrations (YYYYMMDDHHMMSS_*.sql)
tools/            → deploy.sh, bench_aws.py, benchmark.py, new_migration.sh
docs/             → Specs, ADRs, architecture, benchmarks
tests/            → Integration & smoke tests
```

## Quick Start

### Local Development
```bash
docker compose up -d                    # Start Valkey + PostgreSQL
cargo build                             # Build all crates
cargo run --bin sme-api                 # Start worker (queue consumer)
cargo run --bin sme-gateway             # Start gateway (HTTP/WS)
# http://localhost:3001/trading/        # Trading UI
# http://localhost:3001/dashboard/      # Metrics dashboard
```

### AWS Deployment
```bash
tools/deploy.sh                         # Build + push + deploy + run migrations
tools/deploy.sh --migrate-only          # Just run migrations
tools/deploy.sh --skip-build            # Deploy with existing images
```

### Manage Commands (Worker Lambda)
```bash
# Run migrations after deploy
aws lambda invoke --function-name serverless-matching-engine-worker \
  --payload '{"manage":{"command":"run_migrations"}}' /tmp/out.json

# Reset benchmark data
aws lambda invoke --function-name serverless-matching-engine-worker \
  --payload '{"manage":{"command":"reset_all"}}' /tmp/out.json
```

See [docs/aws-architecture.md](docs/aws-architecture.md) for detailed AWS architecture.

## AWS Infrastructure

| Component | Service | Spec |
|-----------|---------|------|
| Cache | ElastiCache Valkey | `cache.t4g.micro`, Valkey 8.1 |
| Database | RDS PostgreSQL | `db.t4g.small`, PG 16.6 |
| Connection Pool | RDS Proxy | `sme-proxy-v2` |
| Gateway | Lambda + API Gateway | 512MB, arm64, Lambda Web Adapter |
| Worker | Lambda | 1024MB, x86_64, native Rust runtime |
| Frontend | CloudFront + S3 | Static HTML/JS/CSS |
| WebSocket | API Gateway WebSocket | Real-time orderbook + trades |

## Performance (c=40, 60s benchmark)

| Metric | Value |
|--------|-------|
| Client dispatch rate | 558 orders/sec |
| Client latency (p50) | 71ms |
| Client latency (p99) | 111ms |
| Lua EVAL (p50) | 1-2ms |
| Errors | 0 |

## Features

→ [`docs/specs/features.md`](docs/specs/features.md)

## Architecture Decisions

→ [`docs/adr/`](docs/adr/)

## Scaling Strategy

→ [`docs/scaling-strategy.md`](docs/scaling-strategy.md)
