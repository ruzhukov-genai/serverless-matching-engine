# AWS Architecture — Serverless Matching Engine

## Overview

Single SAM stack (`serverless-matching-engine`) with 3 nested `AWS::CloudFormation::Stack` substacks.
All templates use `Transform: AWS::Serverless-2016-10-31`.

```
template.yaml (root)
├── stacks/network.yaml   — VPC, subnets, route tables, IGW, security groups
├── stacks/backend.yaml   — EC2, Lambda ×2 (gateway + worker), API Gateway
└── stacks/frontend.yaml  — S3 bucket, CloudFront, OAC
```

## Deploy

```bash
cd infra
sam build --use-buildkit
sam deploy \
  --stack-name serverless-matching-engine \
  --resolve-s3 \
  --capabilities CAPABILITY_IAM CAPABILITY_NAMED_IAM CAPABILITY_AUTO_EXPAND \
  --no-confirm-changeset \
  --parameter-overrides KeyPairName=sme-ec2-key DBPassword=<password>
```

After deploy, upload frontend:
```bash
aws s3 sync web/trading/ s3://<FrontendBucketName>/ --delete
```

## Architecture Diagram

```
                    ┌──────────────────────────┐
                    │       CloudFront          │
                    │   (Frontend S3 Origin)    │
                    └──────────┬───────────────┘
                               │
         ┌─────────────────────┴─────────────────────┐
         │              API Gateway (HTTP)            │
         │   /{proxy+} → Gateway Lambda               │
         └─────────────────────┬─────────────────────┘
                               │
              ┌────────────────┴────────────────┐
              │     Gateway Lambda (arm64)       │
              │  Rust binary via Lambda Web      │
              │  Adapter (lambda-adapter)        │
              │  Reads Dragonfly cache, serves   │
              │  REST + WebSocket, dispatches    │
              │  orders to Worker Lambda         │
              └────────────────┬────────────────┘
                               │ (Lambda invoke)
              ┌────────────────┴────────────────┐
              │     Worker Lambda (arm64)        │
              │  Rust binary, processes single   │
              │  order: lock balance (PG),       │
              │  Lua EVAL match (Dragonfly),     │
              │  persist trades (PG),            │
              │  update cache                    │
              └────────────────┬────────────────┘
                               │
              ┌────────────────┴────────────────┐
              │        EC2 (t4g.micro, arm64)    │
              │   ┌───────────┐ ┌─────────────┐ │
              │   │ PostgreSQL│ │  Dragonfly   │ │
              │   │  (5432)   │ │   (6379)     │ │
              │   └───────────┘ └─────────────┘ │
              └─────────────────────────────────┘
```

## Network Layout

- **VPC:** 10.0.0.0/16
- **Public Subnet A:** 10.0.1.0/24 (us-east-1a) — EC2 lives here
- **Private Subnet A:** 10.0.2.0/24 (us-east-1a) — Lambda ENIs
- **Private Subnet B:** 10.0.3.0/24 (us-east-1b) — Lambda ENIs (AZ redundancy)

Lambda functions are VPC-attached to reach EC2's Dragonfly/PG on private IPs.

## Security Groups

| SG | Inbound | Purpose |
|----|---------|---------|
| EC2 SG | 22 (SSH from SSHCidr), 5432 + 6379 (from Lambda SGs) | EC2 access |
| Lambda SG | None inbound | Gateway Lambda outbound to EC2 |
| Worker Lambda SG | None inbound | Worker Lambda outbound to EC2 |

## Lambda Functions

### Gateway Lambda
- **Runtime:** Docker image (ECR), arm64
- **Memory:** 512 MB
- **Timeout:** 900s (WebSocket support)
- **Handler:** Lambda Web Adapter (runs Rust HTTP server on port 3001)
- **Env vars:** `DRAGONFLY_URL`, `ORDER_DISPATCH_MODE=lambda`, `WORKER_LAMBDA_ARN`
- **VPC:** Private subnets A+B, Lambda SG
- **Key insight:** Uses `lambda-adapter` extension to run a standard HTTP server as a Lambda function. SAM builds via `Dockerfile.gateway`.

### Worker Lambda
- **Runtime:** Docker image (ECR), arm64
- **Memory:** 512 MB
- **Timeout:** 30s
- **Handler:** `bootstrap` (native Rust Lambda runtime via `lambda_runtime` crate)
- **Env vars:** `DATABASE_URL`, `DRAGONFLY_URL`
- **VPC:** Private subnets A+B, Worker Lambda SG

## EC2 Instance

- **Type:** t4g.micro (arm64, 1 vCPU, 1 GB RAM)
- **AMI:** Amazon Linux 2023 (latest arm64)
- **Services:** PostgreSQL 16, Dragonfly 1.28.2
- **IAM Role:** SSM managed instance (for remote management via `aws ssm send-command`)

### EC2 UserData Bootstrap

The UserData script in `stacks/backend.yaml` installs PG + Dragonfly, creates the schema,
seeds data, and configures networking. Key gotchas captured below.

## Gotchas & Lessons Learned

### SAM Nested Stacks
- Use `AWS::CloudFormation::Stack` (not `AWS::Serverless::Application`) for nested stacks
- Each substack must have `Transform: AWS::Serverless-2016-10-31` to use `AWS::Serverless::Function`
- SAM processes the transform in all templates during `sam build`
- Deploy needs `CAPABILITY_IAM CAPABILITY_NAMED_IAM CAPABILITY_AUTO_EXPAND`
- `CAPABILITY_NAMED_IAM` is required when nested stacks create IAM roles with custom names

### Docker Build (arm64 on x86_64)
- SAM's `docker build` passes `--platform linux/arm64` which breaks cross-compile Dockerfiles
- The Dockerfiles use multi-stage: `FROM --platform=linux/amd64` for builder, `FROM --platform=linux/arm64` for runtime
- `sam build --use-buildkit` enables BuildKit which handles this correctly
- QEMU binfmt is NOT needed — the builder stage compiles natively on amd64, only the final runtime stage is arm64

### Amazon Linux 2023 — curl-minimal Conflict
- AL2023 ships `curl-minimal` which conflicts with full `curl` package
- **Fix:** Use `dnf install -y --allowerasing` in UserData
- Without this, the entire UserData script fails silently and no services get installed

### Dragonfly on t4g.micro
- Dragonfly v1.28.2 auto-detects 2 threads but requires 256MB per thread (512MB total)
- t4g.micro has 1GB RAM but with PG running, only ~400MB free
- **Fix:** `--proactor_threads=1` in Dragonfly config + `--maxmemory=256mb`
- Without this: Dragonfly exits with "2 threads, so 512MB required. Exiting..."

### PostgreSQL listen_addresses
- Default PG installs with `listen_addresses = 'localhost'`
- Lambda functions connect via EC2's private IP, not localhost
- **Fix:** `sed -i "s/^#listen_addresses.*/listen_addresses = '*'/" postgresql.conf`
- Also need pg_hba.conf entry: `host matching_engine sme 10.0.0.0/8 md5`
- And `GRANT ALL PRIVILEGES ON ALL TABLES/SEQUENCES IN SCHEMA public TO sme`

### Dragonfly Cache Seeding
- Gateway Lambda warms cache from Dragonfly on cold start (reads `cache:pairs` key)
- New EC2 instance = empty Dragonfly = gateway returns empty pairs
- **Fix:** Seed `cache:pairs` key + `version:{pair_id}` keys after Dragonfly starts
- After seeding, force Gateway Lambda cold start (update any env var)
- On AL2023, redis CLI package is `redis6` and binary is `redis6-cli` (not `redis-cli`)

### Lambda Cold Starts After Config Changes
- Lambda keeps warm execution environments that cache the initial state
- After changing EC2 config (PG listen_addresses, Dragonfly data), existing Lambda instances won't see changes
- **Fix:** Update any Lambda env var (e.g. add `CACHE_REFRESH=<timestamp>`) to force new execution environments

### CloudFormation Stack Deletion
- VPC-attached Lambda ENI cleanup takes 5-15 minutes during stack deletion
- S3 buckets with versioning enabled must have ALL versions + delete markers removed before deletion
- If a stack delete fails on S3 bucket, manually empty it then retry with `--retain-resources`

## Endpoints (Production)

| Endpoint | URL |
|----------|-----|
| API Gateway | `https://<api-id>.execute-api.us-east-1.amazonaws.com` |
| CloudFront | `https://<dist-id>.cloudfront.net` |
| EC2 SSH | `ssh -i ~/.ssh/sme-ec2-key.pem ec2-user@<elastic-ip>` |
| EC2 SSM | `aws ssm send-command --instance-ids <id> ...` |

Check `aws cloudformation describe-stacks --stack-name serverless-matching-engine --query 'Stacks[0].Outputs'` for current values.

## Cost Estimate (idle)

- EC2 t4g.micro: ~$6/mo (free tier eligible)
- Lambda: $0 idle (pay per invocation)
- API Gateway: $0 idle
- CloudFront: ~$0 (minimal traffic)
- S3: ~$0.03/mo
- ECR: ~$0.10/mo (2 images)
- **Total idle:** ~$6-7/mo
