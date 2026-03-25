# AWS Architecture — Serverless Matching Engine

## Overview

Fully serverless architecture using AWS managed services. No EC2 instances needed.

```
template.yaml (root)
├── stacks/network.yaml   — VPC, subnets, route tables, IGW, security groups
├── stacks/backend.yaml   — Lambda ×3 (gateway + worker + ws-handler), RDS, ElastiCache, API Gateway
└── stacks/frontend.yaml  — S3 bucket, CloudFront, OAC
```

## Architecture Diagram

```
                    ┌──────────────────────────┐
                    │       CloudFront          │
                    │   d3ux5yer0uv7b5          │
                    └──────────┬───────────────┘
                               │
         ┌─────────────────────┴─────────────────────┐
         │         API Gateway (HTTP + WebSocket)      │
         │   HTTP: kpvhsf0ub8  WS: 2shnq9yk0c         │
         └─────────────────────┬─────────────────────┘
                               │
              ┌────────────────┴────────────────┐
              │    Gateway Lambda (x86_64)       │
              │  512MB, Rust + Lambda Web Adapter│
              │  Reads Valkey cache, serves      │
              │  REST API, dispatches orders     │
              │  to Worker Lambda (inline await) │
              └────────────────┬────────────────┘
                               │ (async Lambda invoke)
              ┌────────────────┴────────────────┐
              │     Worker Lambda (x86_64)       │
              │  1024MB, native Rust runtime     │
              │  Lua EVAL match (Valkey),        │
              │  persist trades (PG via Proxy),  │
              │  update cache                    │
              └────────┬───────────┬────────────┘
                       │           │
          ┌────────────┴──┐  ┌────┴──────────────┐
          │ ElastiCache    │  │  RDS PostgreSQL    │
          │ Valkey 8.1     │  │  db.t4g.small      │
          │ cache.t4g.micro│  │  via RDS Proxy     │
          └───────────────┘  └────────────────────┘
```

## Infrastructure Components

### ElastiCache Valkey (cache.t4g.micro)
- **Engine:** Valkey 8.1.0
- **Node type:** cache.t4g.micro (2 vCPU, 0.5GB)
- **CF resource:** `ValkeyCache` (ReplicationGroup, single-node, no automatic failover)
- **Purpose:** Order book cache (sorted sets), Lua atomic matching, balance cache,
  pub/sub for real-time feeds, metrics storage
- **Endpoint:** dynamically assigned by CloudFormation (output: `ElastiCacheEndpoint`)

### RDS PostgreSQL (db.t4g.small)
- **Engine:** PostgreSQL 16.6
- **Instance:** `serverless-matching-engine-pg`
- **Max connections:** ~52
- **Purpose:** Durable persistence for orders, trades, balances, audit log

### RDS Proxy
- **Name:** `sme-proxy-v2`
- **Purpose:** Connection pooling for Lambda burst scaling
- **Endpoint:** `sme-proxy-v2.proxy-cp3apgpybbhw.us-east-1.rds.amazonaws.com`
- **Key benefit:** Prevents connection stampede when 40+ Lambdas cold-start simultaneously

### Gateway Lambda
- **Architecture:** x86_64
- **Memory:** 512 MB
- **Timeout:** 30s
- **Handler:** Lambda Web Adapter (buffered mode, runs Rust axum server on port 3001)
- **Env vars:** `REDIS_URL`, `ORDER_DISPATCH_MODE=lambda`, `WORKER_LAMBDA_ARN`
- **VPC:** Private subnets A+B
- **Key insight:** Uses `lambda-adapter` extension to run a standard axum HTTP server
- **Constraint:** `tokio::spawn` does NOT work — LWA freezes runtime after response. All async work must complete inline.

### WS Handler Lambda
- **Architecture:** x86_64
- **Memory:** 256 MB
- **Timeout:** 30s
- **Handler:** `bootstrap` (native Rust Lambda runtime)
- **Purpose:** API Gateway WebSocket $connect/$disconnect/sendMessage
- **Env vars:** `REDIS_URL`
- **VPC:** Private subnets A+B

### Worker Lambda
- **Architecture:** x86_64
- **Memory:** 1024 MB
- **Timeout:** 30s
- **Handler:** `bootstrap` (native Rust Lambda runtime via `lambda_runtime` crate)
- **Env vars:** `DATABASE_URL` (RDS Proxy endpoint), `REDIS_URL` (ElastiCache)
- **VPC:** Private subnets A+B
- **Pool config:** PG max_connections(1), min_connections(0), connect_lazy()
- **Valkey pool:** 2 connections

### API Gateway
- **HTTP API:** `kpvhsf0ub8` — REST proxy to Gateway Lambda
- **WebSocket API:** `2shnq9yk0c` — Real-time orderbook and trade feeds

### CloudFront + S3
- **Distribution:** `d3ux5yer0uv7b5.cloudfront.net`
- **S3 origin:** Static frontend files (trading UI, dashboard)
- **API origin:** Proxies to API Gateway for `/api/*` paths

## Network Layout

- **VPC:** 10.0.0.0/16
- **Private Subnet A:** 10.0.2.0/24 (us-east-1a) — Lambda ENIs, ElastiCache, RDS
- **Private Subnet B:** 10.0.3.0/24 (us-east-1b) — Lambda ENIs (AZ redundancy), RDS

Lambda functions are VPC-attached to reach ElastiCache and RDS on private IPs.

## Deploy Process

### Primary: tools/deploy.sh
```bash
tools/deploy.sh                    # Build + push Docker images + run migrations
tools/deploy.sh --migrate-only     # Just run migrations (no build/deploy)
tools/deploy.sh --skip-build       # Deploy with existing images + migrations
```

### How It Works
`tools/deploy.sh` does:
1. `sam build --parallel` — builds all 3 Docker images (gateway, worker, ws-handler) using `cargo-chef` for dep layer caching
2. `sam deploy` — pushes images to ECR, uploads templates to S3, deploys via CloudFormation changeset
3. Runs `manage:run_migrations` on the worker Lambda after deploy

SAM reads `Metadata.DockerContext` + `Metadata.Dockerfile` from each function in `backend.yaml` to build images.
No manual `docker build` / `docker push` / `update-function-code` needed.

## Manage Commands

Worker Lambda accepts manage commands via direct invocation:

```json
{"manage": {"command": "run_migrations"}}
{"manage": {"command": "reset_all"}}
{"manage": {"command": "reset_balances"}}
{"manage": {"command": "truncate_orders"}}
{"manage": {"command": "exec_sql", "sql": "ALTER TABLE ..."}}
{"manage": {"command": "query", "sql": "SELECT count(*) as cnt FROM orders"}}
```

**Lightweight commands** (run_migrations, exec_sql, query) use a standalone PG connection — no Valkey needed.
**Stateful commands** (reset_all, reset_balances, truncate_orders) initialize full worker state with Valkey.

```bash
aws lambda invoke --function-name serverless-matching-engine-worker \
  --invocation-type RequestResponse \
  --payload '{"manage":{"command":"run_migrations"}}' /tmp/out.json
```

## Migrations

- **Format:** `migrations/YYYYMMDDHHMMSS_description.sql`
- **Runner:** sqlx built-in migrator (`_sqlx_migrations` tracking table)
- **Each file** has `-- migration:` and `-- depends-on:` comment headers
- **Embedded** in Worker Lambda Docker image at `/var/runtime/migrations/`
- **Create new:** `tools/new_migration.sh "add_user_roles"`
- **Run:** `tools/deploy.sh --migrate-only` or `manage:run_migrations`

## Performance (Benchmark: c=40, 60s)

| Metric | Value |
|--------|-------|
| Client dispatch rate | 558 orders/sec |
| Client latency p50/p95/p99 | 71 / 83 / 111 ms |
| Worker Lua EVAL p50 | 1-2ms |
| Worker persist p50 | 860ms |
| Worker total p50 | 2,894ms |
| Cold start (init) | ~1s |
| Errors | 0 |
| Timeouts | 0 |

## Endpoints

| Endpoint | URL |
|----------|-----|
| HTTP API | `https://kpvhsf0ub8.execute-api.us-east-1.amazonaws.com` |
| WebSocket | `wss://2shnq9yk0c.execute-api.us-east-1.amazonaws.com/ws` |
| CloudFront | `https://d3ux5yer0uv7b5.cloudfront.net` |
| Trading UI | `https://d3ux5yer0uv7b5.cloudfront.net/trading/` |

## Cost Estimate (idle)

| Service | Monthly |
|---------|---------|
| RDS db.t4g.small | ~$24 |
| ElastiCache cache.t4g.micro | ~$9 |
| RDS Proxy | ~$22 |
| Lambda | $0 (pay per use) |
| API Gateway | $0 (pay per use) |
| CloudFront | ~$0 |
| S3 + ECR | ~$0.15 |
| **Total idle** | **~$55/mo** |

## Gotchas & Lessons Learned

### Lambda + RDS Proxy
- `connect_lazy()` is essential — `connect()` blocks on TCP handshake; 40+ concurrent Lambdas all blocking = proxy exhaustion
- PG pool `max_connections(1)` for worker Lambda — one Lambda = one order = one connection
- RDS Proxy SG rules need both inbound (from Lambda) AND outbound (to RDS + Secrets Manager)

### Docker Builds
- All 3 Lambdas are x86_64 — no cross-compilation needed on x86_64 build hosts
- SAM uses legacy Docker builder (no BuildKit) — don't use `COPY --chmod`, use `COPY` + `RUN chmod`
- `cargo-chef` caches dependency layers — rebuild after dep changes takes ~15min (full) vs ~1min (code-only)
- `docker system prune -af --volumes` reclaims massive space (78GB+) — run periodically

### Lambda Container Recycling
- SAM deploy automatically creates new image tags — all warm instances get replaced on next invoke
- If using manual `update-function-code`, change any env var to force all containers to recycle

### CloudWatch Logs
- `filter-log-events` has 30-60s lag after Lambda finishes
- Use `describe-log-streams` + `get-log-events` on specific streams for real-time data

### Migrations
- Old `_sqlx_migrations` table from numbered migrations was dropped and recreated with timestamped versions
- All existing migrations are idempotent (IF NOT EXISTS / IF EXISTS)
- Manage command runs migrations BEFORE worker state init (no Valkey dependency)

### Trade FK Constraints
- Dropped via migration 005 — concurrent Lambdas match against each other's orders not yet persisted to PG
- Referential integrity guaranteed by Lua matching engine in Valkey (atomic operations)
