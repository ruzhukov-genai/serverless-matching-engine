# Serverless Matching Engine

> A high-performance, stateless order matching engine in **Rust**, deployed as AWS Lambda.

Fully serverless — inline matching runs inside the Gateway Lambda on every order request. Matching is atomic via Lua EVAL in Valkey. Scales to zero when idle, scales out automatically under load.

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
Client → API Gateway → Gateway Lambda (inline match)
                              │
                    ┌─────────┴─────────┐
                    │    Lua EVAL       │ ← Valkey (ElastiCache)
                    │  (atomic match)   │
                    └─────────┬─────────┘
                              │
                        persist trades
                              │
                       PostgreSQL (RDS via Proxy)
```

**Core flow (inline mode — production default):**
`POST /api/orders` → Gateway validates → Lua EVAL match (Valkey, atomic) → persist (PG) → 201 Created

All matching runs in-process in the Gateway Lambda. No Worker Lambda, no queue.

## Structure

```
crates/
  gateway/        → HTTP/WS gateway Lambda (inline matching + persistence in production)
  shared/         → Types, config, cache (sorted sets + Lua matching), DB, engine
  api/            → Local dev worker (BRPOP queue consumer)
  ws-handler/     → WebSocket API Gateway handler ($connect/$disconnect/sendMessage)
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
cargo run --bin sme-api                 # Start worker (local queue consumer)
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

### Manage Commands (Gateway Lambda — POST /internal/manage)
```bash
# Run migrations after deploy
curl -X POST $API_URL/internal/manage -d '{"command":"run_migrations"}'

# Reset benchmark data (orders + balances) for N users
curl -X POST $API_URL/internal/manage -d '{"command":"reset_all","users":100}'

# Truncate orders/trades + clear book cache
curl -X POST $API_URL/internal/manage -d '{"command":"truncate_orders"}'
```

See [docs/aws-architecture.md](docs/aws-architecture.md) for detailed AWS architecture.

## AWS Infrastructure

| Component | Service | Spec |
|-----------|---------|------|
| Cache | ElastiCache Valkey | `cache.t4g.micro`, Valkey 8.0 |
| Database | RDS PostgreSQL | `db.t4g.small`, PG 16.6 |
| Connection Pool | RDS Proxy | `serverless-matching-engine-proxy` |
| Gateway | Lambda + API Gateway | 1024MB, x86_64, Lambda Web Adapter |
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
