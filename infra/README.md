# SME Infrastructure — AWS Deployment Guide

Cheapest-possible production deployment for the Serverless Matching Engine.
Target cost: **~$8–10/month** at low traffic.

## Architecture

```
                        Internet
                           │
              ┌────────────┼────────────┐
              │            │            │
        CloudFront    API Gateway   SSH (22)
        (CDN/TLS)    HTTP API v2    ↓
              │            │      EC2 t4g.micro
              │            │      ┌─────────────┐
              │            ↓      │ Dragonfly   │
              │         Lambda    │  (6379)     │
              │         arm64     │             │
              │         (sme-     │ PostgreSQL  │
              │          gateway) │  (5432)     │
              │            │      │             │
              │            └─────→│ sme-api     │
              │          (VPC     │ (worker)    │
              ↓           priv.)  └─────────────│
           S3 Bucket              Public Subnet │
           (static UI)            Elastic IP    │
              │                                 │
              └──────── VPC 10.0.0.0/16 ────────┘
```

### Component Roles

| Component | What | Where |
|-----------|------|-------|
| EC2 t4g.micro (ARM) | Dragonfly + PostgreSQL + sme-api worker | Public subnet (Elastic IP) |
| Lambda arm64 | sme-gateway HTTP handler | Private subnets (no internet egress) |
| API Gateway HTTP API v2 | Lambda proxy + CORS | AWS managed |
| CloudFront | CDN for static UI + API proxy | PriceClass_100 (US/EU) |
| S3 | Static frontend files | Private bucket + OAC |
| ECR | Gateway container image | Same region |

### Why This Layout

- **EC2 in public subnet**: needs internet for package installs, ECR pulls, S3 binary download
- **Lambda in private subnet**: only needs to reach EC2's Dragonfly on port 6379 — no internet required → **no NAT Gateway** (saves ~$32/mo)
- **sme-api on EC2**: worker uses persistent BRPOP connection to Dragonfly; co-location eliminates network hops and Lambda cold starts aren't suitable for long-lived queue consumers
- **Lambda Web Adapter**: zero code changes to sme-gateway — LWA translates Lambda invocations to HTTP requests transparently

---

## Cost Breakdown

| Resource | Cost/mo (on-demand) | Notes |
|----------|---------------------|-------|
| EC2 t4g.micro | ~$6.00 | ARM, 1 vCPU, 1GB RAM |
| EBS 20GB gp3 | ~$1.60 | SSD, encrypted |
| Elastic IP | $0.00 | Free when attached to running instance |
| Lambda (512MB arm64) | $0.00–$1.00 | Free tier: 1M req + 400K GB-sec/mo |
| API Gateway HTTP API | ~$1.00/M req | Cheapest API GW type |
| S3 (frontend) | <$0.10 | Static files, very low storage |
| CloudFront | $0.00–$1.00 | Free tier: 1TB + 10M req/mo |
| ECR | ~$0.10 | ~1GB image storage |
| **Total** | **~$8–10/mo** | Low traffic, free tier applies |

**Reserved pricing**: EC2 t4g.micro 1-year reserved = ~$4/mo → **~$6–8/mo total**

---

## Prerequisites

### Tools
- **AWS CLI v2**: `aws --version` (≥ 2.0)
- **Docker**: with `buildx` plugin and QEMU for ARM cross-compilation
- **Rust**: toolchain with `aarch64-unknown-linux-gnu` target (for sme-api cross-compile)

### AWS Setup
1. **Key pair**: create in target region (`aws ec2 create-key-pair --key-name sme-key`)
2. **AWS credentials**: `aws configure` or set `AWS_PROFILE`
3. **Permissions needed**:
   - EC2, VPC, IAM, ECR, Lambda, API Gateway, S3, CloudFront, CloudFormation

### Install QEMU for ARM cross-compilation
```bash
# macOS
docker run --privileged --rm tonistiigi/binfmt --install arm64

# Linux (Ubuntu)
sudo apt-get install -y qemu-user-static binfmt-support
docker run --privileged --rm tonistiigi/binfmt --install arm64
```

---

## Quick Start

### 1. Clone and configure
```bash
git clone https://github.com/ruzhukov-genai/serverless-matching-engine
cd serverless-matching-engine

# Configure (create .env or export directly)
export AWS_REGION=us-east-1
export KEY_PAIR_NAME=sme-key          # Must exist in AWS
export SSH_CIDR=YOUR.IP.ADDRESS/32    # Restrict SSH to your IP
export DB_PASSWORD=your-secure-password
```

### 2. Deploy everything
```bash
./infra/deploy.sh \
  --region us-east-1 \
  --key-pair sme-key \
  --ssh-cidr YOUR.IP/32
```

The script will:
1. Build the gateway Docker image (ARM cross-compile)
2. Push to ECR
3. Deploy backend CloudFormation stack
4. Deploy frontend CloudFormation stack
5. Upload web files to S3
6. Invalidate CloudFront cache

### 3. Deploy sme-api worker binary
The EC2 instance is ready but needs the `sme-api` binary:

```bash
# Cross-compile for ARM
rustup target add aarch64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu --package sme-api

# Get EC2 IP from CloudFormation output
EC2_IP=$(aws cloudformation describe-stacks \
  --stack-name sme-backend \
  --query 'Stacks[0].Outputs[?OutputKey==`EC2PublicIP`].OutputValue' \
  --output text)

# Deploy
scp -i ~/.ssh/sme-key.pem \
  target/aarch64-unknown-linux-gnu/release/sme-api \
  ec2-user@${EC2_IP}:/tmp/

ssh -i ~/.ssh/sme-key.pem ec2-user@${EC2_IP} \
  'sudo mv /tmp/sme-api /usr/local/bin/ && sudo systemctl restart sme-api && sudo systemctl status sme-api'
```

**Alternative**: Upload to S3 and set `SmeApiBinaryS3Uri` parameter:
```bash
aws s3 cp target/aarch64-unknown-linux-gnu/release/sme-api s3://your-bucket/sme-api
export SME_API_S3_URI=s3://your-bucket/sme-api
./infra/deploy.sh ...
```

---

## Manual Deployment (step by step)

### Backend stack
```bash
# Create ECR repo
aws ecr create-repository --repository-name serverless-matching-engine/sme-gateway

# Build + push image
ECR_URI=$(aws ecr describe-repositories \
  --repository-names serverless-matching-engine/sme-gateway \
  --query 'repositories[0].repositoryUri' --output text)

aws ecr get-login-password | docker login --username AWS --password-stdin \
  $(echo $ECR_URI | cut -d/ -f1)

docker buildx build --platform linux/arm64 \
  -f infra/Dockerfile.gateway \
  -t ${ECR_URI}:latest --push .

# Deploy
aws cloudformation deploy \
  --template-file infra/cfn-backend.yaml \
  --stack-name sme-backend \
  --capabilities CAPABILITY_NAMED_IAM \
  --parameter-overrides \
    KeyPairName=sme-key \
    SSHCidr=YOUR.IP/32 \
    GatewayImageUri=${ECR_URI}:latest \
    DBPassword=your-password
```

### Frontend stack
```bash
API_URL=$(aws cloudformation describe-stacks \
  --stack-name sme-backend \
  --query 'Stacks[0].Outputs[?OutputKey==`ApiGatewayUrl`].OutputValue' \
  --output text)

aws cloudformation deploy \
  --template-file infra/cfn-frontend.yaml \
  --stack-name sme-frontend \
  --parameter-overrides ApiGatewayUrl=${API_URL}

# Upload frontend
S3_BUCKET=$(aws cloudformation describe-stacks \
  --stack-name sme-frontend \
  --query 'Stacks[0].Outputs[?OutputKey==`FrontendBucketName`].OutputValue' \
  --output text)

aws s3 sync web/ s3://${S3_BUCKET} --delete
```

---

## Updating

### Update gateway only (code change)
```bash
# Rebuild + push image
docker buildx build --platform linux/arm64 -f infra/Dockerfile.gateway \
  -t ${ECR_URI}:latest --push .

# Update Lambda to use new image
aws lambda update-function-code \
  --function-name serverless-matching-engine-gateway \
  --image-uri ${ECR_URI}:latest
```

### Update sme-api binary
```bash
cargo build --release --target aarch64-unknown-linux-gnu --package sme-api

scp -i ~/.ssh/sme-key.pem \
  target/aarch64-unknown-linux-gnu/release/sme-api \
  ec2-user@${EC2_IP}:/tmp/

ssh -i ~/.ssh/sme-key.pem ec2-user@${EC2_IP} \
  'sudo mv /tmp/sme-api /usr/local/bin/ && sudo systemctl restart sme-api'
```

### Update frontend
```bash
aws s3 sync web/ s3://${S3_BUCKET} --delete

CF_ID=$(aws cloudformation describe-stacks \
  --stack-name sme-frontend \
  --query 'Stacks[0].Outputs[?OutputKey==`CloudFrontDistributionId`].OutputValue' \
  --output text)

aws cloudfront create-invalidation --distribution-id ${CF_ID} --paths "/*"
```

---

## Environment Variables

### Lambda (sme-gateway)
| Variable | Value | Notes |
|----------|-------|-------|
| `DRAGONFLY_URL` | `redis://10.0.x.x:6379` | EC2 private IP, set by CloudFormation |
| `PORT` | `3001` | sme-gateway listen port |
| `AWS_LWA_PORT` | `3001` | Lambda Web Adapter port |
| `AWS_LWA_READINESS_CHECK_PATH` | `/api/pairs` | LWA polls until 200 OK |
| `RUST_LOG` | `info` | Log level |

### EC2 (sme-api)
| Variable | Value | Notes |
|----------|-------|-------|
| `DATABASE_URL` | `postgres://sme:pw@localhost:5432/matching_engine` | Set in systemd unit |
| `DRAGONFLY_URL` | `redis://localhost:6379` | Co-located on EC2 |
| `RUST_LOG` | `info` | Log level |

---

## Monitoring & Debugging

### Check service status (SSH to EC2)
```bash
ssh -i ~/.ssh/sme-key.pem ec2-user@${EC2_IP}

# Service status
sudo systemctl status sme-api dragonfly postgresql

# Live logs
sudo journalctl -u sme-api -f
sudo journalctl -u dragonfly -f

# Dragonfly health
redis-cli -p 6379 ping
redis-cli -p 6379 info memory

# PostgreSQL
psql -U sme -d matching_engine -c "\dt"
```

### Lambda logs
```bash
aws logs tail /aws/lambda/serverless-matching-engine-gateway --follow
```

### Bootstrap log
```bash
ssh -i ~/.ssh/sme-key.pem ec2-user@${EC2_IP} 'sudo tail -100 /var/log/sme-userdata.log'
```

---

## CloudFormation Stacks

### cfn-backend.yaml
Creates:
- VPC (10.0.0.0/16) with 2 AZs
- Public subnets (EC2), private subnets (Lambda)
- Internet Gateway, route tables
- Security groups (EC2, Lambda)
- EC2 t4g.micro + Elastic IP + IAM role
- Lambda function (arm64, 512MB, VPC)
- API Gateway HTTP API v2
- ECR repository

### cfn-frontend.yaml
Creates:
- S3 bucket (private + versioned)
- CloudFront OAC
- CloudFront distribution (PriceClass_100)
  - S3 origin for static files (24h cache)
  - API Gateway origin for `/api/*` (no cache)

---

## ADR Compliance

This deployment respects all Architecture Decision Records:

- **ADR-001 (Stateless Workers)**: sme-api on EC2 is stateless — all state in Dragonfly/PG ✓
- **ADR-002 (CacheBroadcasts)**: Lambda Web Adapter keeps process warm between invocations; CacheBroadcasts work within the container lifecycle ✓
- **ADR-003 (Async PG)**: DB writes off hot path — unchanged ✓
- **ADR-004 (Lua Matching)**: Dragonfly configured with `--default_lua_flags=allow-undeclared-keys` ✓
- **ADR-005 (Per-pair queues)**: sme-api worker on EC2 creates per-pair consumers with Semaphore(3) ✓

---

## Teardown

```bash
# Delete stacks (in reverse order)
aws cloudformation delete-stack --stack-name sme-frontend
aws cloudformation delete-stack --stack-name sme-backend

# Empty S3 bucket first (CloudFormation can't delete non-empty buckets)
S3_BUCKET=$(aws cloudformation describe-stacks \
  --stack-name sme-frontend \
  --query 'Stacks[0].Outputs[?OutputKey==`FrontendBucketName`].OutputValue' \
  --output text)
aws s3 rm s3://${S3_BUCKET} --recursive

# Then delete
aws cloudformation delete-stack --stack-name sme-frontend
```
