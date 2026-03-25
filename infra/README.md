# SME Infrastructure — AWS SAM Deployment

Fully serverless deployment using AWS Lambda, RDS PostgreSQL, and ElastiCache Valkey.
Estimated cost: **~$55/month** idle.

## Architecture

Single SAM stack with 3 nested substacks:

- **Gateway Lambda** (arm64, Docker/ECR) — HTTP/WS via Lambda Web Adapter, dispatches orders
- **Worker Lambda** (x86_64, Docker/ECR) — processes orders: Lua EVAL match (Valkey), persist (PG)
- **RDS PostgreSQL** (db.t4g.small) — durable persistence via RDS Proxy
- **ElastiCache Valkey** (cache.t4g.micro) — order book cache, Lua matching, pub/sub
- **API Gateway** — HTTP API + WebSocket API
- **CloudFront + S3** — static frontend

See [docs/aws-architecture.md](../docs/aws-architecture.md) for detailed architecture.

## Quick Deploy

```bash
# Recommended: use the deploy script
tools/deploy.sh                    # Build + push + deploy + run migrations
tools/deploy.sh --migrate-only     # Just run migrations
tools/deploy.sh --skip-build       # Deploy with existing images

# Full stack deploy (when infrastructure changes)
cd infra
sam build --use-buildkit
sam deploy --capabilities CAPABILITY_IAM CAPABILITY_NAMED_IAM CAPABILITY_AUTO_EXPAND
```

## Stack Structure

| File | Purpose |
|------|---------|
| `template.yaml` | Root SAM template with 3 nested stacks |
| `stacks/network.yaml` | VPC, subnets, security groups |
| `stacks/backend.yaml` | Lambda functions, RDS, ElastiCache, API Gateway |
| `stacks/frontend.yaml` | S3, CloudFront distribution |
| `Dockerfile.gateway` | Gateway Lambda container (arm64) |
| `Dockerfile.worker` | Worker Lambda container (x86_64) |

## Manage Commands

```bash
# Run migrations
aws lambda invoke --function-name serverless-matching-engine-worker \
  --payload '{"manage":{"command":"run_migrations"}}' /tmp/out.json

# Reset benchmark data
aws lambda invoke --function-name serverless-matching-engine-worker \
  --payload '{"manage":{"command":"reset_all"}}' /tmp/out.json
```

## Monitoring

```bash
# Lambda logs
aws logs tail /aws/lambda/serverless-matching-engine-worker --follow
aws logs tail /aws/lambda/serverless-matching-engine-gateway --follow

# Stack outputs
aws cloudformation describe-stacks --stack-name serverless-matching-engine \
  --query 'Stacks[0].Outputs'
```

## Teardown

```bash
sam delete --stack-name serverless-matching-engine
```

Note: Empty S3 buckets manually before deletion.
