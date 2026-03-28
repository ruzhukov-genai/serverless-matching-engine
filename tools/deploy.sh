#!/bin/bash
set -euo pipefail
# =============================================================================
# deploy.sh — Build, deploy, and run post-deploy tasks
#
# Uses SAM to build Docker images and deploy the entire stack.
# sam build: builds Docker images locally (no --use-containers)
# sam deploy: pushes images to ECR, deploys CloudFormation stack
#
# Usage:
#   tools/deploy.sh                    # Full deploy (sam build + deploy + migrations)
#   tools/deploy.sh --skip-build       # Deploy without rebuilding images
#   tools/deploy.sh --migrate-only     # Just run migrations (no build/deploy)
#   tools/deploy.sh --build-only       # Just build (no deploy)
# =============================================================================

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
INFRA_DIR="$PROJECT_ROOT/infra"

STACK_NAME="serverless-matching-engine"
REGION="${AWS_REGION:-us-east-1}"
GATEWAY_LAMBDA="${STACK_NAME}-gateway"

SKIP_BUILD=false
MIGRATE_ONLY=false
BUILD_ONLY=false

for arg in "$@"; do
    case $arg in
        --skip-build)    SKIP_BUILD=true ;;
        --migrate-only)  MIGRATE_ONLY=true ;;
        --build-only)    BUILD_ONLY=true ;;
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

    if echo "$body" | python3 -c "import sys,json; d=json.load(sys.stdin); exit(0 if d.get('status')=='ok' else 1)" 2>/dev/null; then
        log "  ✓ ${cmd} complete"
    else
        err "  ✗ ${cmd} failed: $body"
    fi
}

# ── Run migrations ───────────────────────────────────────────────────
run_migrations() {
    log "Running database migrations via ${GATEWAY_LAMBDA}..."
    invoke_manage "$GATEWAY_LAMBDA" '{"manage":{"command":"run_migrations"}}'
}

if $MIGRATE_ONLY; then
    run_migrations
    log "Done."
    exit 0
fi

# ── SAM Build ────────────────────────────────────────────────────────
if ! $SKIP_BUILD; then
    log "Building with SAM (Docker images, no containers)..."
    cd "$INFRA_DIR"
    sam build --parallel
    log "Build complete."
fi

if $BUILD_ONLY; then
    log "Build only — skipping deploy."
    exit 0
fi

# ── SAM Deploy ───────────────────────────────────────────────────────
log "Deploying with SAM..."
cd "$INFRA_DIR"
sam deploy \
    --no-confirm-changeset \
    --no-fail-on-empty-changeset \
    --capabilities CAPABILITY_IAM CAPABILITY_NAMED_IAM CAPABILITY_AUTO_EXPAND \
    --resolve-image-repos \
    --resolve-s3

# ── Post-deploy: Run migrations ─────────────────────────────────────
run_migrations

log "Deploy complete."
