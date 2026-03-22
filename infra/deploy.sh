#!/usr/bin/env bash
# =============================================================================
# deploy.sh — Build, push, and deploy the SME stack
#
# Uses docker buildx for cross-compile (arm64 on amd64), pushes to ECR,
# then deploys the CloudFormation stack via SAM CLI.
#
# Usage:
#   ./deploy.sh              # Full build + push + deploy
#   ./deploy.sh --skip-build # Push existing images + deploy
#   ./deploy.sh --deploy-only # Deploy only (images already in ECR)
# =============================================================================
set -euo pipefail

REGION="${AWS_REGION:-us-east-1}"
ACCOUNT_ID=$(aws sts get-caller-identity --query 'Account' --output text)
ECR_BASE="${ACCOUNT_ID}.dkr.ecr.${REGION}.amazonaws.com"
PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INFRA_DIR="${PROJECT_ROOT}/infra"

GATEWAY_REPO="serverless-matching-engine/sme-gateway"
WORKER_REPO="serverless-matching-engine/sme-worker"
GATEWAY_URI="${ECR_BASE}/${GATEWAY_REPO}:latest"
WORKER_URI="${ECR_BASE}/${WORKER_REPO}:latest"

STACK_NAME="serverless-matching-engine-backend"

SKIP_BUILD=false
DEPLOY_ONLY=false
for arg in "$@"; do
  case "$arg" in
    --skip-build)  SKIP_BUILD=true ;;
    --deploy-only) DEPLOY_ONLY=true ;;
  esac
done

# ── Step 1: Build Docker images ───────────────────────────────────────────
if [[ "$SKIP_BUILD" == false && "$DEPLOY_ONLY" == false ]]; then
  echo "=== Step 1/3: Building Docker images (cross-compile arm64) ==="

  echo "→ Building gateway..."
  docker buildx build \
    --platform linux/arm64 \
    --provenance=false \
    -f "${INFRA_DIR}/Dockerfile.gateway" \
    -t "${GATEWAY_URI}" \
    "${PROJECT_ROOT}"

  echo "→ Building worker..."
  docker buildx build \
    --platform linux/arm64 \
    --provenance=false \
    -f "${INFRA_DIR}/Dockerfile.worker" \
    -t "${WORKER_URI}" \
    "${PROJECT_ROOT}"

  echo "=== Docker images built ==="
fi

# ── Step 2: Push to ECR ──────────────────────────────────────────────────
if [[ "$DEPLOY_ONLY" == false ]]; then
  echo "=== Step 2/3: Pushing images to ECR ==="

  aws ecr get-login-password --region "${REGION}" | \
    docker login --username AWS --password-stdin "${ECR_BASE}"

  for repo in "${GATEWAY_REPO}" "${WORKER_REPO}"; do
    aws ecr describe-repositories --repository-names "${repo}" --region "${REGION}" 2>/dev/null || \
      aws ecr create-repository --repository-name "${repo}" --region "${REGION}" \
        --image-scanning-configuration scanOnPush=false
  done

  echo "→ Pushing gateway..."
  docker push "${GATEWAY_URI}"

  echo "→ Pushing worker..."
  docker push "${WORKER_URI}"

  echo "=== Images pushed ==="
fi

# ── Step 3: SAM deploy (CloudFormation stack update) ──────────────────────
echo "=== Step 3/3: Deploying stack ==="
cd "${INFRA_DIR}"

aws cloudformation deploy \
  --template-file template.yaml \
  --stack-name "${STACK_NAME}" \
  --region "${REGION}" \
  --capabilities CAPABILITY_IAM CAPABILITY_NAMED_IAM \
  --parameter-overrides \
    "KeyPairName=GmindRZKeyPair" \
    "DBPassword=sme_prod_9b43c1802d8440e9666be882d925d933" \
    "GatewayImageUri=${GATEWAY_URI}" \
    "WorkerImageUri=${WORKER_URI}" \
  --no-fail-on-empty-changeset

echo ""
echo "=== Deploy complete ==="
echo ""
echo "Stack outputs:"
aws cloudformation describe-stacks \
  --stack-name "${STACK_NAME}" \
  --region "${REGION}" \
  --query 'Stacks[0].Outputs[*].[OutputKey,OutputValue]' \
  --output table
