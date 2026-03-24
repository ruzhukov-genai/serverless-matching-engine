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
WS_HANDLER_REPO="serverless-matching-engine/sme-ws-handler"
GATEWAY_URI="${ECR_BASE}/${GATEWAY_REPO}:latest"
WORKER_URI="${ECR_BASE}/${WORKER_REPO}:latest"
WS_HANDLER_URI="${ECR_BASE}/${WS_HANDLER_REPO}:latest"

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

  echo "→ Building ws-handler..."
  docker buildx build \
    --platform linux/arm64 \
    --provenance=false \
    -f "${INFRA_DIR}/Dockerfile.ws-handler" \
    -t "${WS_HANDLER_URI}" \
    "${PROJECT_ROOT}"

  echo "=== Docker images built ==="
fi

# ── Step 2: Push to ECR ──────────────────────────────────────────────────
if [[ "$DEPLOY_ONLY" == false ]]; then
  echo "=== Step 2/3: Pushing images to ECR ==="

  aws ecr get-login-password --region "${REGION}" | \
    docker login --username AWS --password-stdin "${ECR_BASE}"

  for repo in "${GATEWAY_REPO}" "${WORKER_REPO}" "${WS_HANDLER_REPO}"; do
    aws ecr describe-repositories --repository-names "${repo}" --region "${REGION}" 2>/dev/null || \
      aws ecr create-repository --repository-name "${repo}" --region "${REGION}" \
        --image-scanning-configuration scanOnPush=false
  done

  echo "→ Pushing gateway..."
  docker push "${GATEWAY_URI}"

  echo "→ Pushing worker..."
  docker push "${WORKER_URI}"

  echo "→ Pushing ws-handler..."
  docker push "${WS_HANDLER_URI}"

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

# ── Step 4: Generate frontend config.js from stack outputs ────────────────────
echo "=== Generating frontend config.js ==="

API_URL=$(aws cloudformation describe-stacks \
  --stack-name "${STACK_NAME}" --region "${REGION}" \
  --query 'Stacks[0].Outputs[?OutputKey==`ApiGatewayUrl`].OutputValue' --output text)

WS_URL=$(aws cloudformation describe-stacks \
  --stack-name "${STACK_NAME}" --region "${REGION}" \
  --query 'Stacks[0].Outputs[?OutputKey==`WebSocketUrl`].OutputValue' --output text)

CF_DIST_ID=$(aws cloudformation describe-stacks \
  --stack-name "${STACK_NAME}" --region "${REGION}" \
  --query 'Stacks[0].Outputs[?OutputKey==`CloudFrontDistributionId`].OutputValue' --output text)

CF_URL=$(aws cloudformation describe-stacks \
  --stack-name "${STACK_NAME}" --region "${REGION}" \
  --query 'Stacks[0].Outputs[?OutputKey==`CloudFrontUrl`].OutputValue' --output text)

BUCKET=$(aws cloudformation describe-stacks \
  --stack-name "${STACK_NAME}" --region "${REGION}" \
  --query 'Stacks[0].Outputs[?OutputKey==`FrontendBucketName`].OutputValue' --output text)

# Write config.js with embedded URLs (CloudFront proxies /api/*, so API_URL = origin)
cat > "${PROJECT_ROOT}/web/trading/config.js" <<CFGEOF
// Auto-generated by deploy.sh — DO NOT EDIT
window.SME_CONFIG = {
    API_URL: null,
    WS_URL: '${WS_URL}',
};
CFGEOF

echo "  API_URL: (same origin via CloudFront)"
echo "  WS_URL:  ${WS_URL}"

# ── Step 5: Upload frontend to S3 + invalidate CloudFront ────────────────────
if [[ -n "${BUCKET}" ]]; then
  echo "=== Uploading frontend to S3 ==="
  aws s3 sync "${PROJECT_ROOT}/web/trading/" "s3://${BUCKET}/trading/" --delete
  aws s3 sync "${PROJECT_ROOT}/web/dashboard/" "s3://${BUCKET}/dashboard/" --delete 2>/dev/null || true
  echo '<html><head><meta http-equiv="refresh" content="0; url=/trading/"></head></html>' | \
    aws s3 cp - "s3://${BUCKET}/index.html" --content-type "text/html"

  if [[ -n "${CF_DIST_ID}" ]]; then
    echo "=== Invalidating CloudFront cache ==="
    aws cloudfront create-invalidation --distribution-id "${CF_DIST_ID}" --paths "/*" \
      --query 'Invalidation.Status' --output text
  fi

  echo ""
  echo "Frontend: ${CF_URL}/trading/"
fi
