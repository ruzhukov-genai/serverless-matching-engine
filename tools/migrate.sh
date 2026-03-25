#!/bin/bash

# Usage: tools/migrate.sh [DATABASE_URL]
# Default: uses DATABASE_URL env var

set -euo pipefail

DATABASE_URL="${1:-${DATABASE_URL:-}}"

if [[ -z "$DATABASE_URL" ]]; then
    echo "ERROR: DATABASE_URL not provided as argument or env var"
    echo "Usage: $0 [DATABASE_URL]"
    echo "Example: $0 postgres://user:pass@host:5432/dbname"
    exit 1
fi

echo "Running migrations against: $DATABASE_URL"

# Build the api binary (which has access to shared crate with migrations)
cargo build --release --bin sme-api

# Set DATABASE_URL and run a minimal migration command
# We'll call the API binary with a special --migrate flag
DATABASE_URL="$DATABASE_URL" ./target/release/sme-api --migrate

echo "Migrations completed successfully"