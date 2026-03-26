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
         └──────┬──────────────────────┬─────────────┘
                │ POST /api/orders      │ all other routes
                │ (sqs-direct mode)     │
      ┌─────────┴───────┐   ┌──────────┴──────────────┐
      │   SQS Standard   │   │  Gateway Lambda (x86_64) │
      │ orders            │   │  512MB, LWA (buffered)   │
      │ (CF-managed)     │   │  Reads Valkey cache,     │
      └─────────┬───────┘   │  serves REST API         │
                │ ESM        └─────────────────────────┘
                │ batch 10, MaxConcurrency=50
              ┌─┴──────────────────────────────┐
              │     Worker Lambda (x86_64)      │
              │  1024MB, 120s, native Rust      │
              │  validate + Lua EVAL match,     │
              │  persist trades (PG via Proxy), │
              │  update cache                   │
              └────────┬───────────┬────────────┘
                       │           │
          ┌────────────┴──┐  ┌────┴──────────────┐
          │ ElastiCache    │  │  RDS PostgreSQL    │
          │ Valkey 8.0     │  │  db.t4g.small      │
          │ cache.t4g.micro│  │  via RDS Proxy     │
          │ (sme-valkey,   │  │  (sme-proxy-v2)    │
          │  manual)       │  │                    │
          └───────────────┘  └────────────────────┘
```

**Note:** Diagram shows the current `sqs-direct` deployment. In `queue` mode, API GW routes all requests through Gateway Lambda, which does LPUSH to Valkey; EventBridge `rate(1 minute)` triggers Worker drain. In `sqs` mode, Gateway Lambda sends to SQS instead of Valkey.

## Infrastructure Components

### ElastiCache Valkey (cache.t4g.micro)
- **Engine:** Valkey 8.0
- **Node type:** cache.t4g.micro (2 vCPU, 0.5GB)
- **Cluster name:** `sme-valkey`
- **CF resource:** ~~`ValkeyCache`~~ — **not CF-managed**. The cluster was accidentally deleted when `ValkeyCache` was removed from `backend.yaml`. It was manually recreated with no TLS and no automatic failover.
- **Endpoint:** `sme-valkey.rmmdxf.ng.0001.use1.cache.amazonaws.com` (CF Parameter `ValkeyEndpoint`, not `!GetAtt`)
- **TLS:** Disabled (ElastiCache defaults to TLS-on for Valkey 8.0; must explicitly disable at creation)
- **Purpose:** Order book cache (sorted sets), Lua atomic matching, balance cache,
  pub/sub for real-time feeds, metrics storage

### RDS PostgreSQL (db.t4g.small)
- **Engine:** PostgreSQL 16.6
- **Instance:** `serverless-matching-engine-pg`
- **Max connections:** ~52
- **Purpose:** Durable persistence for orders, trades, balances, audit log

### RDS Proxy
- **Name:** `sme-proxy-v2`
- **Purpose:** Connection pooling for Lambda burst scaling
- **Endpoint:** `sme-proxy-v2.proxy-cp3apgpybbhw.us-east-1.rds.amazonaws.com` (CF Parameter `RDSProxyEndpoint`, not `!GetAtt`)
- **Key benefit:** Prevents connection stampede when 40+ Lambdas cold-start simultaneously
- **SG note:** Proxy shares SG `sg-09224456f3c4aa7ea` with the RDS instance. The SG requires a self-referencing egress rule on port 5432; without it the Proxy can authenticate clients but cannot reach RDS backend.

### Gateway Lambda
- **Architecture:** x86_64
- **Memory:** 512 MB
- **Timeout:** 30s
- **Handler:** Lambda Web Adapter (buffered mode, runs Rust axum server on port 3001)
- **Env vars:** `REDIS_URL`, `ORDER_DISPATCH_MODE` (`queue` / `sqs` / `sqs-direct`)
- **VPC:** Private subnets A+B
- **Key insight:** Uses `lambda-adapter` extension to run a standard axum HTTP server
- **Constraint:** `tokio::spawn` does NOT work — LWA freezes runtime after response. All async work must complete inline.
- **Note (sqs-direct):** In `sqs-direct` mode, Gateway Lambda is NOT in the `POST /api/orders` path. It still handles all other routes via the `$default` route.

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
- **Timeout:** 120s (increased from 30s to safely process SQS batches within VisibilityTimeout)
- **Handler:** `bootstrap` (native Rust Lambda runtime via `lambda_runtime` crate)
- **Env vars:** `DATABASE_URL` (RDS Proxy endpoint), `REDIS_URL` (ElastiCache), `ORDER_DISPATCH_MODE`
- **VPC:** Private subnets A+B
- **Pool config:** PG max_connections(1), min_connections(0), connect_lazy()
- **Valkey pool:** 2 connections
- **SQS trigger:** Event source mapping, batch size 10, MaxConcurrency=50, batching window 1s
- **Timestamps:** Uses SQS `attributes.SentTimestamp` for `received_at` (true queue arrival time); `persisted_at` set by Postgres `NOW()`
- **Validation (sqs-direct):** Worker generates UUID if `id` missing, accepts `time_in_force` alias for `tif`, validates quantity/price constraints. Cache update failures are non-fatal (warn-and-continue).

### SQS Standard Queue
- **Name:** `serverless-matching-engine-orders`
- **Type:** Standard (switched from FIFO on 2026-03-26 — FIFO serialized to ~20 ord/s per MessageGroupId)
- **CF-managed:** Yes, conditional on `OrderDispatchMode=sqs-direct` parameter
- **VisibilityTimeout:** 180s (must be ≥ Worker Lambda timeout)
- **API GW integration:** Native AWS service integration routes `POST /api/orders` directly to SQS (bypasses Gateway Lambda)
- **Why Standard:** Atomicity guaranteed by Valkey Lua EVAL, not message ordering. Standard enables parallel Lambda consumption with MaxConcurrency=50 → ~170-230 ord/s drain.

### API Gateway
- **HTTP API:** `kpvhsf0ub8` — REST proxy to Gateway Lambda (`$default` route) + native SQS integration (`POST /api/orders` in sqs-direct mode)
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

**VPC Endpoints (CF-managed Interface endpoints):**
- `com.amazonaws.us-east-1.lambda` — Lambda SDK calls (gateway → worker in `sqs`/`lambda` modes)
- `com.amazonaws.us-east-1.sts` — IAM token refresh
- `com.amazonaws.us-east-1.sqs` — SQS API access from Lambda (required for Worker SQS ESM in VPC)

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

## Performance (Benchmarks as of March 26, 2026)

### Sync mode (direct Lambda dispatch)

| Concurrency | Match p50 | Persist p50 | Errors |
|-------------|-----------|-------------|--------|
| c=1 | ~17ms | 0.3ms | 0 |
| c=10 | ~17ms | ~1ms | 0 |
| c=40 | ~17ms | 2.7ms | 0 |

Match latency (Lua EVAL) is stable at ~17ms across all concurrency levels. Persist latency scales mildly with concurrency due to PG row-level locking.

### SQS-direct mode (Standard SQS, MaxConcurrency=50)

**Dispatch (API GW → SQS, zero Lambda in path):**

| Concurrency | Rate | p50 | p95 | Errors |
|-------------|------|-----|-----|--------|
| c=10 | 868/s | 9ms | 28ms | 0 |
| c=50 | 2,345/s | 19ms | 32ms | 0 |
| c=100 | 2,327/s | 39ms | 80ms | 0 |

**Processing (Worker Lambda drain):**
- ~170-230 orders/sec with MaxConcurrency=50
- Match p50=2ms (Lua EVAL), Persist p50=8ms (RDS via Proxy)
- **True E2E (includes queue wait):** p50=~3min, p99=~6min when dispatch >> drain

**Comparison: FIFO vs Standard SQS:**

| Metric | FIFO | Standard |
|--------|------|----------|
| Dispatch c=10 | 319/s, 58% errors | 868/s, 0 errors |
| Dispatch c=50 | 301/s, 85% errors | 2,345/s, 0 errors |
| Processing | ~20 ord/s | ~170-230 ord/s |

### Queue mode (Valkey LPUSH + EventBridge drain)

| Metric | Value |
|--------|-------|
| Gateway ceiling | ~220 ord/s |
| Worker drain rate | ~100 ord/s (5 parallel Lambda × 20 ord/s) |
| EventBridge schedule | `rate(1 minute)` |

**Note on cache update errors (fixed):** At high concurrency, Worker's post-match cache update was causing ~13% error rate. Fixed by making cache update non-fatal (warn-and-continue). Stale cache is acceptable — next successful order refresh wins.

### Configurable user count
- `reset_all`/`reset_balances` accept `{"users": N}` (default 100)
- 100 users vs 10 users: persist p99 dropped from 29ms → 15ms (less balance contention)

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
| RDS Proxy (`sme-proxy-v2`) | ~$22 |
| SQS Standard | ~$0 (pay per use, ~$0.40/1M requests) |
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
- **RDS Proxy SG egress trap** — if Proxy shares SG with RDS instance and the SG has egress locked to `127.0.0.1/32`, the Proxy can authenticate clients but can't reach RDS. Fix: add self-referencing egress rule on port 5432 to the shared SG.

### Docker Builds
- All 3 Lambdas are x86_64 — no cross-compilation needed on x86_64 build hosts
- SAM uses legacy Docker builder (no BuildKit) — don't use `COPY --chmod`, use `COPY` + `RUN chmod`
- `cargo-chef` caches dependency layers — rebuild after dep changes takes ~15min (full) vs ~1min (code-only)
- `docker system prune -af --volumes` reclaims massive space (78GB+) — run periodically
- **SAM build always regenerates templates from source** — manual edits to the built `template.yaml` are overwritten. Always edit `infra/stacks/*.yaml`.

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

### CloudFormation Resource Deletion Trap
- **Removing a resource from a CF/SAM template causes CF to delete the physical resource** on next deploy.
- This happened with `ValkeyCache` — removing it from `backend.yaml` destroyed the ElastiCache cluster.
- Mitigation: before removing a CF resource, either (a) add `DeletionPolicy: Retain`, or (b) move the resource's config to a Parameter before the deploy that removes it.
- `sme-valkey` cluster was manually recreated; `ValkeyEndpoint` and `RDSProxyEndpoint` are now CF Parameters.

### ElastiCache / Valkey
- **TLS enabled by default on Valkey 8.0** — AWS enables `TransitEncryptionEnabled=true` by default. If the Redis client URL doesn't have TLS (`rediss://`), connection will fail silently.
- **AWS CLI v1 doesn't support `--no-transit-encryption-enabled`** — use `--transit-encryption-enabled false` or create via Console.

### SQS + Lambda ESM
- **SQS VisibilityTimeout must be ≥ Lambda timeout** — if the Lambda takes longer than VisibilityTimeout, the message becomes visible again and is processed a second time.
- **`sqs-direct` returns raw XML** — API Gateway's native SQS integration returns the SQS `SendMessage` XML response to the client, not a clean JSON 202. Clients need to handle this.
- **SQS VPC endpoint required** — Lambda ESM for SQS in a VPC requires a VPC endpoint for `sqs`. Without it, Lambda can't poll the queue.
- **Standard SQS over FIFO for parallel processing** — FIFO serializes per MessageGroupId (~20 ord/s). Atomicity is guaranteed by Valkey Lua EVAL, not message ordering. Standard SQS with MaxConcurrency=50 → ~170-230 ord/s.
- **PurgeQueue has 60s cooldown** — can't purge more than once per minute.
- **In-flight messages survive purge** — messages with active VisibilityTimeout (180s) are not purged. To fully clear: disable ESM → wait for visibility timeout → purge → wait 65s → re-enable ESM.
- **`received_at` must use SQS SentTimestamp** — if set by the worker at processing time, queue wait is invisible. Use `attributes.SentTimestamp` from the SQS record for true arrival time.
