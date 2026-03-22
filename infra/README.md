# SME Infrastructure — AWS SAM Deployment

Serverless deployment of the matching engine using AWS Lambda, backed by PostgreSQL and Dragonfly on EC2.
Estimated cost: **~$8–10/month** at low traffic.

## Architecture

The system deploys as a single SAM stack with 3 nested substacks:

- **Gateway Lambda** (arm64, Docker/ECR) — serves HTTP/WS via Lambda Web Adapter, dispatches orders to Worker Lambda
- **Worker Lambda** (arm64, Docker/ECR) — processes individual orders: lock balance (PG), Lua EVAL match (Dragonfly), persist trades (PG)
- **EC2 t4g.micro** (arm64) — runs PostgreSQL 16 + Dragonfly 1.28.2 ONLY (no sme-api binary)
- **API Gateway HTTP API v2** — Lambda proxy
- **CloudFront** — S3 frontend origin + API origin
- **S3** — static frontend files

For detailed architecture diagrams and decisions, see [docs/aws-architecture.md](../docs/aws-architecture.md).

## Prerequisites

- **AWS CLI v2** with configured credentials
- **Docker** with Buildx for ARM cross-compilation
- **SAM CLI**: `pip install aws-sam-cli`

Install ARM emulation for cross-compilation:
```bash
# macOS/Linux
docker run --privileged --rm tonistiigi/binfmt --install arm64
```

## Quick Deploy

```bash
cd infra
sam build --use-buildkit
sam deploy --capabilities CAPABILITY_IAM CAPABILITY_NAMED_IAM CAPABILITY_AUTO_EXPAND
```

The deploy will:
1. Build Gateway and Worker Lambda Docker images (ARM cross-compile)
2. Create ECR repositories and push images
3. Deploy network, backend, and frontend stacks
4. Upload web files to S3
5. Output CloudFront URL for testing

## Configuration

Default parameters are in `template.yaml`. Override via `--parameter-overrides`:

```bash
sam deploy \
  --capabilities CAPABILITY_IAM CAPABILITY_NAMED_IAM CAPABILITY_AUTO_EXPAND \
  --parameter-overrides \
    Environment=staging \
    KeyPairName=my-keypair \
    SSHCidr=1.2.3.4/32 \
    DBPassword=secure-password
```

## Stack Structure

| File | Purpose |
|------|---------|
| `template.yaml` | Root SAM template with 3 nested stacks |
| `stacks/network.yaml` | VPC, subnets, security groups |
| `stacks/backend.yaml` | EC2, Lambda functions, API Gateway |
| `stacks/frontend.yaml` | S3, CloudFront distribution |
| `Dockerfile.gateway` | Gateway Lambda container build |
| `Dockerfile.worker` | Worker Lambda container build |

## Monitoring

### Service Status
SSH to EC2 instance (IP from stack outputs):
```bash
ssh -i ~/.ssh/keypair.pem ec2-user@EC2_IP
sudo systemctl status postgresql dragonfly
```

### Lambda Logs
```bash
sam logs --stack-name serverless-matching-engine-backend --name GatewayFunction --tail
sam logs --stack-name serverless-matching-engine-backend --name WorkerFunction --tail
```

### Testing
After deployment, test via CloudFront URL:
```bash
# Get URL from stack outputs
CLOUDFRONT_URL=$(aws cloudformation describe-stacks \
  --stack-name serverless-matching-engine-frontend \
  --query 'Stacks[0].Outputs[?OutputKey==`CloudFrontUrl`].OutputValue' \
  --output text)

curl ${CLOUDFRONT_URL}/api/pairs
```

## Updates

### Code Changes
```bash
cd infra
sam build --use-buildkit
sam deploy
```

### Frontend Only
```bash
cd infra
sam sync --stack-name serverless-matching-engine-frontend --watch
```

## Teardown

```bash
sam delete --stack-name serverless-matching-engine
```

Note: May need to empty S3 buckets manually if they contain files.