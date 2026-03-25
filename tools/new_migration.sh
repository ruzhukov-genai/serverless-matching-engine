#!/bin/bash
set -euo pipefail
# =============================================================================
# new_migration.sh — Create a new timestamped migration file
#
# Usage:
#   tools/new_migration.sh "add_user_roles"
# =============================================================================

if [ $# -ne 1 ]; then
    echo "Usage: $0 <description>"
    echo "  description: snake_case name (e.g., add_user_roles)"
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
MIGRATIONS_DIR="$(cd "$SCRIPT_DIR/../migrations" && pwd)"
DESC="$1"
TIMESTAMP=$(date -u +%Y%m%d%H%M%S)
VERSION="${TIMESTAMP}_${DESC}"
FILENAME="${VERSION}.sql"

# Find the previous migration (last file alphabetically)
PREV=$(ls "$MIGRATIONS_DIR"/*.sql 2>/dev/null | sort | tail -1 | xargs -I{} basename {} .sql)

if [ -z "$PREV" ]; then
    DEPENDS="none"
else
    DEPENDS="$PREV"
fi

cat > "$MIGRATIONS_DIR/$FILENAME" << EOF
-- migration: ${VERSION}
-- depends-on: ${DEPENDS}
-- TODO: describe what this migration does

-- Write your SQL here
EOF

echo "Created: migrations/${FILENAME}"
echo "  depends-on: ${DEPENDS}"
echo ""
echo "Edit the file, then deploy with: tools/deploy.sh"
