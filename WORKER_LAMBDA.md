# Worker Lambda

## Overview

The Worker Lambda (`crates/worker-lambda/`) processes individual orders as a stateless Rust binary
running on AWS Lambda (x86_64, 1024MB, 30s timeout).

### Order Flow
```
Gateway Lambda → async invoke → Worker Lambda
  1. Parse order JSON from Lambda event
  2. Load pairs cache (from Valkey, cached after first cold start)
  3. Lock balance in PostgreSQL (atomic row-level lock)
  4. Execute matching via Lua EVAL in Valkey (atomic)
  5. Persist trades + updated orders to PostgreSQL (batch INSERT)
  6. Update cache keys (orderbook, trades, ticker, portfolio)
  7. Return success/failure to Lambda runtime
```

### Cold Start Optimization
- PG pool: `max_connections(1)`, `min_connections(0)`, `connect_lazy()` — no blocking TCP on init
- Valkey pool: 2 connections
- Cache seed: skipped if `cache:pairs` already exists (first Lambda seeds, others skip)
- Cold start time: ~1s (init 54ms + first PG connection)

## Manage Commands

All admin operations via direct Lambda invocation with `{"manage": {"command": "..."}}`:

| Command | Description | Needs Valkey? |
|---------|-------------|:---:|
| `run_migrations` | Run pending SQL migrations | No |
| `exec_sql` | Execute arbitrary DDL/DML | No |
| `query` | Run SELECT, return `cnt` column | No |
| `reset_all` | Truncate orders + reset balances + sync cache | Yes |
| `reset_balances` | Reset 10 test users to initial balances | Yes |
| `truncate_orders` | Truncate orders + clear orderbook cache | Yes |

Lightweight commands use a standalone PG connection (run before `get_state()`).
Stateful commands initialize full worker state (PG + Valkey).

## Deployment

```bash
# Automated
tools/deploy.sh

# Manual
DOCKER_BUILDKIT=1 docker build --provenance=false -f infra/Dockerfile.worker -t sme-worker:latest .
docker tag sme-worker:latest <ECR_URI>:latest
docker push <ECR_URI>:latest
# Then update Lambda code via boto3 or AWS CLI
```

## Configuration

| Variable | Value | Source |
|----------|-------|--------|
| `REDIS_URL` | `redis://sme-valkey...cache.amazonaws.com:6379` | ElastiCache endpoint |
| `DATABASE_URL` | `postgres://...proxy...rds.amazonaws.com:5432/matching_engine` | RDS Proxy endpoint |
| `RUST_LOG` | `info` | CloudFormation |

## Performance (c=40 benchmark, 2026-03-25)

| Metric | Value |
|--------|-------|
| Lua EVAL (p50) | 1-2ms |
| PG persist (p50) | 860ms |
| Worker total (p50) | 2,894ms |
| Cold start | ~1s |
| Timeouts | 0 |

**Remaining bottleneck:** PG row-level lock contention on `balances` table UPDATE.
40 Lambdas competing to update the same 10 test users' balance rows.

---

**Last updated:** 2026-03-25
