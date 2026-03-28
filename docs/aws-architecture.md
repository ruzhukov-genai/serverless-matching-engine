# AWS Architecture — Serverless Matching Engine

## Overview

Fully serverless. No EC2. Orders are matched inline in the Gateway Lambda.

```
template.yaml (root)
├── stacks/network.yaml   — VPC, subnets, route tables, security groups, VPC endpoints
├── stacks/backend.yaml   — Lambda ×2 (gateway + ws-handler), RDS, ElastiCache, API Gateway
└── stacks/frontend.yaml  — S3 bucket, CloudFront, OAC
```

## Architecture Diagram

```
              ┌──────────────────────────────┐
              │         CloudFront            │
              │   d3ux5yer0uv7b5              │
              └──────────────┬───────────────┘
                             │
         ┌───────────────────┴───────────────────┐
         │       API Gateway (HTTP + WebSocket)    │
         │   HTTP: kpvhsf0ub8  WS: 2shnq9yk0c     │
         └──────────────────┬────────────────────┘
                            │ all routes
              ┌─────────────┴───────────────────┐
              │      Gateway Lambda (x86_64)      │
              │   1024MB, LWA (buffered mode)      │
              │                                    │
              │  POST /api/orders:                 │
              │    validate → Lua EVAL match       │
              │    → PG persist → 201              │
              │                                    │
              │  All other routes:                 │
              │    orderbook, trades, portfolio,   │
              │    WS feeds, manage               │
              └────────────┬──────────┬────────────┘
                           │          │
           ┌───────────────┴──┐  ┌───┴────────────────┐
           │  ElastiCache      │  │  RDS PostgreSQL     │
           │  Valkey 8.0       │  │  db.t4g.small       │
           │  cache.t4g.micro  │  │  via RDS Proxy      │
           │  (sme-valkey)     │  │                     │
           └───────────────────┘  └─────────────────────┘
```

**WS Handler Lambda** (`2shnq9yk0c`) handles WebSocket `$connect`/`$disconnect`/`sendMessage` routes separately.

## Infrastructure Components

### Gateway Lambda
- **Architecture:** x86_64, 1024 MB, 30s timeout
- **Mode:** Lambda Web Adapter (buffered), runs axum HTTP server on port 3001
- **Dispatch:** `ORDER_DISPATCH_MODE=inline` — matching runs in-process (Lua EVAL → PG persist)
- **VPC:** Private subnets A+B
- **Key constraint:** `tokio::spawn` does NOT work — LWA freezes runtime after HTTP response. All work must complete before handler returns.
- **Env vars:** `DATABASE_URL` (RDS Proxy), `REDIS_URL` (ElastiCache), `ORDER_DISPATCH_MODE=inline`

### WS Handler Lambda
- **Architecture:** x86_64, 256 MB, 30s timeout
- **Mode:** Native Rust Lambda runtime
- **Purpose:** WebSocket API Gateway $connect/$disconnect/sendMessage
- **Env vars:** `REDIS_URL`

### ElastiCache Valkey (cache.t4g.micro)
- **Engine:** Valkey 8.0, no TLS, no automatic failover
- **Cluster:** `sme-valkey` — **manually created** (not CF-managed; was accidentally deleted when removed from CF template; recreated manually)
- **Endpoint:** `sme-valkey.rmmdxf.ng.0001.use1.cache.amazonaws.com` (CF Parameter, not `!GetAtt`)
- **Purpose:** Order book cache (sorted sets), Lua atomic matching, balance locks, pub/sub for WS feeds

### RDS PostgreSQL (db.t4g.small)
- **Engine:** PostgreSQL 16.6, ~52 max connections
- **Instance:** `serverless-matching-engine-pg`
- **Purpose:** Durable persistence — orders, trades, balances, audit log

### RDS Proxy
- **Name:** `serverless-matching-engine-proxy` (CF-managed)
- **Endpoint:** CF Parameter `RDSProxyEndpoint`
- **Purpose:** Connection pooling — prevents connection stampede on Lambda burst scaling
- **SG note:** Proxy SG must have self-referencing egress on port 5432 so Proxy ENI can reach RDS ENI

### API Gateway
- **HTTP API:** `kpvhsf0ub8` — proxies all routes to Gateway Lambda
- **WebSocket API:** `2shnq9yk0c` — routes $connect/$disconnect/sendMessage to WS Handler Lambda

### CloudFront + S3
- **Distribution:** `d3ux5yer0uv7b5.cloudfront.net`
- **S3:** Static frontend (trading UI, dashboard)
- **CloudFront Function:** Rewrites `/path/` → `/path/index.html` (directory index rewrite)

## Network Layout

- **VPC:** 10.0.0.0/16
- **Private Subnet A:** 10.0.2.0/24 (us-east-1a) — Lambda ENIs, ElastiCache, RDS
- **Private Subnet B:** 10.0.3.0/24 (us-east-1b) — Lambda ENIs (AZ redundancy), RDS

**VPC Endpoints (CF-managed Interface):**
- `com.amazonaws.us-east-1.lambda` — Lambda SDK (retained in CF, unused in inline mode)
- `com.amazonaws.us-east-1.sts` — IAM token refresh

## Deploy Process

```bash
tools/deploy.sh                    # Build Docker images → push to ECR → SAM deploy → run_migrations
tools/deploy.sh --migrate-only     # Just run migrations (no build/deploy)
tools/deploy.sh --skip-build       # Deploy with existing images + run migrations
```

`sam build` caches Docker layers via `cargo-chef`. Only changed crates rebuild.
**If code changes aren't deploying:** `rm -rf infra/.aws-sam && tools/deploy.sh` (force full rebuild).

## Manage Commands

All admin ops via HTTP: `POST /internal/manage`

```bash
BASE="https://kpvhsf0ub8.execute-api.us-east-1.amazonaws.com"

curl -X POST $BASE/internal/manage -d '{"command":"run_migrations"}'
curl -X POST $BASE/internal/manage -d '{"command":"reset_all","num_users":100}'
curl -X POST $BASE/internal/manage -d '{"command":"reset_balances","num_users":100}'
curl -X POST $BASE/internal/manage -d '{"command":"truncate_orders"}'
curl -X POST $BASE/internal/manage -d '{"command":"query","sql":"SELECT count(*) FROM orders"}'
curl -X POST $BASE/internal/manage -d '{"command":"exec_sql","sql":"ALTER TABLE ..."}'
```

## Migrations

- **Format:** `migrations/YYYYMMDDHHMMSS_description.sql`
- **Runner:** sqlx built-in migrator (`_sqlx_migrations` table)
- **Embedded** in Gateway Lambda Docker image; run via `manage:run_migrations` after deploy
- **Create:** `tools/new_migration.sh "description"`

## Performance (2026-03-28, inline mode)

| Concurrency | Orders/s | E2E p50 | E2E p99 | Match p50 | Persist p50 |
|-------------|----------|---------|---------|-----------|-------------|
| c=1         | 13.8/s   | 4.7ms   | 12.2ms  | 1.3ms     | 3.4ms       |
| c=10        | 128.6/s  | 4.9ms   | 11.7ms  | 1.7ms     | 3.3ms       |
| c=20        | 201.6/s  | 5.8ms   | 26.2ms  | 1.7ms     | 4.0ms       |
| c=40        | 217.7/s  | 8.1ms   | 59.5ms  | 1.8ms     | 6.1ms       |

Bottleneck: RDS Proxy / PG write contention. Lua EVAL (match) stays stable at 1.3–1.8ms.

## Endpoints

| | URL |
|--|-----|
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
| Lambda, API GW, CloudFront | ~$0 (pay per use) |
| S3 + ECR | ~$0.15 |
| **Total idle** | **~$55/mo** |

## Gotchas

### SAM Build Cache
- `sam build` caches Docker layers — if code changes aren't showing up in Lambda, delete `.aws-sam/` and rebuild
- SAM always regenerates template files from source — edit `infra/stacks/*.yaml`, not the built output

### Lambda Web Adapter
- `tokio::spawn` does NOT work for background tasks — LWA freezes runtime after HTTP response
- All async work (matching + persist) must complete before the handler returns

### CloudFormation Resource Deletion Trap
- Removing a resource from a CF template **deletes the physical resource** on next deploy
- `sme-valkey` was lost this way — recreated manually; endpoint now a CF Parameter
- Before removing a sensitive resource: add `DeletionPolicy: Retain` first, then remove

### RDS Proxy SG Egress
- Proxy SG must have a self-referencing egress rule on port 5432
- Without it: Proxy authenticates clients but can't reach RDS backend → timeouts

### ElastiCache Valkey TLS
- Valkey 8.0 defaults to TLS-on — must explicitly disable at creation
- AWS CLI v1 doesn't support `--no-transit-encryption-enabled`; use Console or `--transit-encryption-enabled false`

### VPC Endpoint SGs
- All calling Lambda SGs must be in VPC endpoint inbound rules
- If a new Lambda SG is added by CF, update the endpoint SGs or connections will timeout with "dispatch failure"

### get_orderbook reads Valkey directly
- Each Lambda instance has its own in-memory cache — pub/sub delivery between instances is async/racy
- `GET /api/orderbook/{pair}` reads `cache:orderbook:{pair}` from Valkey directly for consistency
- In-memory cache is only used for WS fan-out
