#!/usr/bin/env bash
# =============================================================================
# deploy.sh — SME Deployment Helper
#
# Usage:
#   ./infra/deploy.sh [--region us-east-1] [--profile default] [--stack-name sme]
#
# Steps:
#   1. Build sme-gateway Docker image (cross-compile for ARM/aarch64)
#   2. Push to ECR
#   3. Deploy cfn-backend.yaml (VPC, EC2, Lambda, API Gateway)
#   4. Deploy cfn-frontend.yaml (S3, CloudFront)
#   5. Upload frontend static files to S3
#   6. Invalidate CloudFront cache
#
# Prerequisites:
#   - AWS CLI v2 (aws configure OR AWS_PROFILE set)
#   - Docker with buildx + QEMU for cross-compilation
#   - Cargo + Rust (for native builds — Docker build handles cross-compile)
#   - Key pair already exists in target region
# =============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "${SCRIPT_DIR}")"

# ---------------------------------------------------------------------------
# Defaults (override via env or flags)
# ---------------------------------------------------------------------------
AWS_REGION="${AWS_REGION:-us-east-1}"
AWS_PROFILE="${AWS_PROFILE:-default}"
STACK_PREFIX="${STACK_PREFIX:-sme}"
PROJECT_NAME="${PROJECT_NAME:-serverless-matching-engine}"
KEY_PAIR_NAME="${KEY_PAIR_NAME:-}"
SSH_CIDR="${SSH_CIDR:-0.0.0.0/0}"
DB_PASSWORD="${DB_PASSWORD:-$(openssl rand -base64 24 | tr -d '/+=')}"
SME_API_S3_URI="${SME_API_S3_URI:-}"
CORS_ORIGIN="${CORS_ORIGIN:-*}"

BACKEND_STACK="${STACK_PREFIX}-backend"
FRONTEND_STACK="${STACK_PREFIX}-frontend"

# ---------------------------------------------------------------------------
# Parse flags
# ---------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --region)    AWS_REGION="$2"; shift 2 ;;
    --profile)   AWS_PROFILE="$2"; shift 2 ;;
    --stack)     STACK_PREFIX="$2"; BACKEND_STACK="${STACK_PREFIX}-backend"; FRONTEND_STACK="${STACK_PREFIX}-frontend"; shift 2 ;;
    --key-pair)  KEY_PAIR_NAME="$2"; shift 2 ;;
    --ssh-cidr)  SSH_CIDR="$2"; shift 2 ;;
    --help|-h)
      grep '^#' "$0" | head -20
      exit 0
      ;;
    *) echo "Unknown flag: $1"; exit 1 ;;
  esac
done

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
log()  { echo "▶ $*"; }
ok()   { echo "✓ $*"; }
fail() { echo "✗ $*" >&2; exit 1; }

aws_cmd() { aws --region "${AWS_REGION}" --profile "${AWS_PROFILE}" "$@"; }

require() {
  for cmd in "$@"; do
    command -v "${cmd}" &>/dev/null || fail "Required: ${cmd} not found in PATH"
  done
}

# ---------------------------------------------------------------------------
# Validate prerequisites
# ---------------------------------------------------------------------------
log "Checking prerequisites..."
require aws docker

aws_cmd sts get-caller-identity > /dev/null || fail "AWS credentials not configured"
ACCOUNT_ID=$(aws_cmd sts get-caller-identity --query Account --output text)
ok "AWS account: ${ACCOUNT_ID} (region: ${AWS_REGION})"

if [[ -z "${KEY_PAIR_NAME}" ]]; then
  # Try to list existing key pairs and pick the first one
  KEY_PAIR_NAME=$(aws_cmd ec2 describe-key-pairs --query 'KeyPairs[0].KeyName' --output text 2>/dev/null || echo "")
  if [[ -z "${KEY_PAIR_NAME}" || "${KEY_PAIR_NAME}" == "None" ]]; then
    fail "KEY_PAIR_NAME not set and no key pairs found in ${AWS_REGION}. Create one first or set --key-pair"
  fi
  log "Using key pair: ${KEY_PAIR_NAME}"
fi

# =============================================================================
# Step 1: Ensure ECR repository exists (or get URI from CloudFormation)
# =============================================================================
log "Checking ECR repository..."
ECR_URI="${ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com/${PROJECT_NAME}/sme-gateway"

# Try to create repo; ignore error if exists
aws_cmd ecr create-repository \
  --repository-name "${PROJECT_NAME}/sme-gateway" \
  --image-scanning-configuration scanOnPush=true \
  2>/dev/null || true

ok "ECR repository: ${ECR_URI}"

# =============================================================================
# Step 2: Build sme-gateway Docker image (ARM/aarch64)
# =============================================================================
log "Building sme-gateway Docker image for ARM (aarch64)..."
cd "${PROJECT_ROOT}"

# Ensure buildx builder with ARM support exists
docker buildx inspect sme-builder &>/dev/null || \
  docker buildx create --name sme-builder --driver docker-container --use

docker buildx use sme-builder
docker run --privileged --rm tonistiigi/binfmt --install arm64 2>/dev/null || true

IMAGE_TAG="${ECR_URI}:latest"
GIT_SHA=$(git -C "${PROJECT_ROOT}" rev-parse --short HEAD 2>/dev/null || echo "local")
IMAGE_TAG_SHA="${ECR_URI}:${GIT_SHA}"

log "Building ${IMAGE_TAG}..."
docker buildx build \
  --platform linux/arm64 \
  --file infra/Dockerfile.gateway \
  --tag "${IMAGE_TAG}" \
  --tag "${IMAGE_TAG_SHA}" \
  --load \
  .

ok "Image built: ${IMAGE_TAG}"

# =============================================================================
# Step 3: Push to ECR
# =============================================================================
log "Authenticating to ECR..."
aws_cmd ecr get-login-password | \
  docker login --username AWS --password-stdin "${ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com"

log "Pushing image to ECR..."
docker push "${IMAGE_TAG}"
docker push "${IMAGE_TAG_SHA}"
ok "Image pushed: ${IMAGE_TAG}"

# =============================================================================
# Step 4: Deploy backend CloudFormation stack
# =============================================================================
log "Deploying backend stack: ${BACKEND_STACK}..."

BACKEND_TEMPLATE="${SCRIPT_DIR}/cfn-backend.yaml"
[[ -f "${BACKEND_TEMPLATE}" ]] || fail "Template not found: ${BACKEND_TEMPLATE}"

aws_cmd cloudformation deploy \
  --template-file "${BACKEND_TEMPLATE}" \
  --stack-name "${BACKEND_STACK}" \
  --capabilities CAPABILITY_NAMED_IAM \
  --parameter-overrides \
    ProjectName="${PROJECT_NAME}" \
    KeyPairName="${KEY_PAIR_NAME}" \
    SSHCidr="${SSH_CIDR}" \
    GatewayImageUri="${IMAGE_TAG}" \
    DBPassword="${DB_PASSWORD}" \
    SmeApiBinaryS3Uri="${SME_API_S3_URI}" \
    AllowedCorsOrigin="${CORS_ORIGIN}" \
  --tags "Project=${PROJECT_NAME}"

ok "Backend stack deployed"

# Fetch outputs
API_URL=$(aws_cmd cloudformation describe-stacks \
  --stack-name "${BACKEND_STACK}" \
  --query 'Stacks[0].Outputs[?OutputKey==`ApiGatewayUrl`].OutputValue' \
  --output text)

EC2_IP=$(aws_cmd cloudformation describe-stacks \
  --stack-name "${BACKEND_STACK}" \
  --query 'Stacks[0].Outputs[?OutputKey==`EC2PublicIP`].OutputValue' \
  --output text)

ok "API Gateway URL: ${API_URL}"
ok "EC2 Public IP:   ${EC2_IP}"

# =============================================================================
# Step 5: Deploy frontend CloudFormation stack
# =============================================================================
log "Deploying frontend stack: ${FRONTEND_STACK}..."

FRONTEND_TEMPLATE="${SCRIPT_DIR}/cfn-frontend.yaml"
[[ -f "${FRONTEND_TEMPLATE}" ]] || fail "Template not found: ${FRONTEND_TEMPLATE}"

aws_cmd cloudformation deploy \
  --template-file "${FRONTEND_TEMPLATE}" \
  --stack-name "${FRONTEND_STACK}" \
  --parameter-overrides \
    ProjectName="${PROJECT_NAME}" \
    ApiGatewayUrl="${API_URL}" \
  --tags "Project=${PROJECT_NAME}"

ok "Frontend stack deployed"

# Fetch frontend outputs
CF_URL=$(aws_cmd cloudformation describe-stacks \
  --stack-name "${FRONTEND_STACK}" \
  --query 'Stacks[0].Outputs[?OutputKey==`CloudFrontUrl`].OutputValue' \
  --output text)

CF_ID=$(aws_cmd cloudformation describe-stacks \
  --stack-name "${FRONTEND_STACK}" \
  --query 'Stacks[0].Outputs[?OutputKey==`CloudFrontDistributionId`].OutputValue' \
  --output text)

S3_BUCKET=$(aws_cmd cloudformation describe-stacks \
  --stack-name "${FRONTEND_STACK}" \
  --query 'Stacks[0].Outputs[?OutputKey==`FrontendBucketName`].OutputValue' \
  --output text)

ok "CloudFront URL: ${CF_URL}"
ok "S3 Bucket:      ${S3_BUCKET}"

# =============================================================================
# Step 6: Upload frontend files to S3
# =============================================================================
log "Uploading frontend files to S3..."

WEB_DIR="${PROJECT_ROOT}/web"
if [[ -d "${WEB_DIR}" ]]; then
  # Upload with appropriate cache headers
  # HTML: short cache (5 min) — may change on deploy
  aws_cmd s3 sync "${WEB_DIR}" "s3://${S3_BUCKET}" \
    --exclude "*" \
    --include "*.html" \
    --cache-control "max-age=300, public" \
    --delete

  # JS/CSS: longer cache (24h)
  aws_cmd s3 sync "${WEB_DIR}" "s3://${S3_BUCKET}" \
    --exclude "*" \
    --include "*.js" \
    --include "*.css" \
    --cache-control "max-age=86400, public" \
    --delete

  # Other static assets (images, fonts, etc.): 7 days
  aws_cmd s3 sync "${WEB_DIR}" "s3://${S3_BUCKET}" \
    --exclude "*.html" \
    --exclude "*.js" \
    --exclude "*.css" \
    --cache-control "max-age=604800, public" \
    --delete

  ok "Frontend files uploaded to s3://${S3_BUCKET}"
else
  log "No web/ directory found — skipping S3 upload"
fi

# =============================================================================
# Step 7: Invalidate CloudFront cache
# =============================================================================
log "Invalidating CloudFront cache..."
INVALIDATION_ID=$(aws_cmd cloudfront create-invalidation \
  --distribution-id "${CF_ID}" \
  --paths "/*" \
  --query 'Invalidation.Id' \
  --output text)
ok "CloudFront invalidation created: ${INVALIDATION_ID}"

# =============================================================================
# Summary
# =============================================================================
echo ""
echo "============================================================"
echo " SME Deployment Complete"
echo "============================================================"
echo ""
echo " Frontend (CloudFront):  ${CF_URL}"
echo " API Gateway:            ${API_URL}"
echo " EC2 SSH:                ssh -i ~/.ssh/${KEY_PAIR_NAME}.pem ec2-user@${EC2_IP}"
echo " S3 Bucket:              ${S3_BUCKET}"
echo ""
echo " Trading UI:  ${CF_URL}/trading/"
echo " Dashboard:   ${CF_URL}/dashboard/"
echo " API:         ${API_URL}/api/pairs"
echo ""
echo " DB Password: ${DB_PASSWORD}"
echo " (save this to a secrets manager!)"
echo ""
echo " sme-api binary: upload to EC2 if not deployed via S3"
echo "   cargo build --release --target aarch64-unknown-linux-gnu --package sme-api"
echo "   scp target/aarch64-unknown-linux-gnu/release/sme-api ec2-user@${EC2_IP}:/tmp/"
echo "   ssh ec2-user@${EC2_IP} 'sudo mv /tmp/sme-api /usr/local/bin/ && sudo systemctl restart sme-api'"
echo ""
