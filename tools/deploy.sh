#!/bin/bash
set -euo pipefail
# =============================================================================
# deploy.sh — Build, deploy, and run post-deploy tasks
#
# Usage:
#   tools/deploy.sh                    # Full deploy (build + sam deploy + migrations)
#   tools/deploy.sh --skip-build       # Deploy with existing images
#   tools/deploy.sh --migrate-only     # Just run migrations (no deploy)
# =============================================================================

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

STACK_NAME="serverless-matching-engine"
REGION="${AWS_REGION:-us-east-1}"
ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text 2>/dev/null)
ECR_BASE="${ACCOUNT_ID}.dkr.ecr.${REGION}.amazonaws.com/${STACK_NAME}"
WORKER_LAMBDA="${STACK_NAME}-worker"

SKIP_BUILD=false
MIGRATE_ONLY=false

for arg in "$@"; do
    case $arg in
        --skip-build)   SKIP_BUILD=true ;;
        --migrate-only) MIGRATE_ONLY=true ;;
    esac
done

# ── Helpers ──────────────────────────────────────────────────────────
log() { echo "▸ $*"; }
err() { echo "✗ $*" >&2; exit 1; }

invoke_manage() {
    local func="$1"
    local payload="$2"
    local cmd
    cmd=$(echo "$payload" | python3 -c 'import sys,json; print(json.load(sys.stdin)["manage"]["command"])')
    log "Invoking manage:${cmd} on ${func}..."

    local tmpfile
    tmpfile=$(mktemp /tmp/lambda-result-XXXXX.json)

    aws lambda invoke \
        --function-name "$func" \
        --invocation-type RequestResponse \
        --payload "$payload" \
        "$tmpfile" > /dev/null 2>&1

    local body
    body=$(cat "$tmpfile")
    rm -f "$tmpfile"

    # Check for FunctionError in the response body
    if echo "$body" | python3 -c "import sys,json; d=json.load(sys.stdin); exit(0 if d.get('status')=='ok' else 1)" 2>/dev/null; then
        log "  ✓ ${cmd} complete"
    else
        err "  ✗ ${cmd} failed: $body"
    fi
}

# ── Run migrations ───────────────────────────────────────────────────
run_migrations() {
    log "Running database migrations via ${WORKER_LAMBDA}..."
    invoke_manage "$WORKER_LAMBDA" '{"manage":{"command":"run_migrations"}}'
}

if $MIGRATE_ONLY; then
    run_migrations
    log "Done."
    exit 0
fi

# ── Build Docker images ─────────────────────────────────────────────
if ! $SKIP_BUILD; then
    log "Building worker Lambda image..."
    DOCKER_BUILDKIT=1 docker build --provenance=false \
        -f infra/Dockerfile.worker -t sme-worker:latest .

    log "Building gateway Lambda image..."
    DOCKER_BUILDKIT=1 docker build --provenance=false \
        -f infra/Dockerfile.gateway -t sme-gateway:latest .

    # Push to ECR
    log "Logging into ECR..."
    aws ecr get-login-password --region "$REGION" | \
        docker login --username AWS --password-stdin "${ACCOUNT_ID}.dkr.ecr.${REGION}.amazonaws.com" 2>/dev/null

    log "Pushing worker image..."
    docker tag sme-worker:latest "${ECR_BASE}/sme-worker:latest"
    docker push "${ECR_BASE}/sme-worker:latest"

    log "Pushing gateway image..."
    docker tag sme-gateway:latest "${ECR_BASE}/sme-gateway:latest"
    docker push "${ECR_BASE}/sme-gateway:latest"
fi

# ── SAM Deploy ───────────────────────────────────────────────────────
# log "Running sam deploy..."
# cd infra && sam deploy --no-confirm-changeset && cd ..
log "NOTE: sam deploy skipped — run manually if stack changes are needed"

# ── Post-deploy: Run migrations ─────────────────────────────────────
run_migrations

log "Deploy complete."
