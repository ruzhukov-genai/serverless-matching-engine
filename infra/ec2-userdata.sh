#!/bin/bash
# =============================================================================
# ec2-userdata.sh — EC2 Bootstrap for SME Backend
#
# Installs and configures:
#   - Dragonfly (Redis-compatible, ARM build) on port 6379
#   - PostgreSQL 16 on port 5432
#   - sme-api worker process
#
# Environment variables (set by CloudFormation UserData or manually):
#   DB_PASSWORD        — PostgreSQL password for sme user
#   SME_API_S3_URI     — S3 URI for sme-api binary (e.g. s3://bucket/sme-api)
#                        Leave empty to skip download (manual install)
#   PROJECT            — Project name tag (default: serverless-matching-engine)
#
# Usage (manual): DB_PASSWORD=secret bash ec2-userdata.sh
# =============================================================================
set -euo pipefail
exec > /var/log/sme-userdata.log 2>&1
echo "=== SME EC2 Bootstrap started at $(date) ==="

DB_PASSWORD="${DB_PASSWORD:-sme_prod_change_me}"
SME_API_S3_URI="${SME_API_S3_URI:-}"
PROJECT="${PROJECT:-serverless-matching-engine}"
DRAGONFLY_VERSION="1.14.2"
ARCH="aarch64"

# =============================================================================
# 1. System updates + packages
# =============================================================================
echo "--- System update ---"
dnf update -y
dnf install -y \
  postgresql16 \
  postgresql16-server \
  wget \
  curl \
  tar \
  unzip \
  awscli \
  htop \
  jq

# =============================================================================
# 2. PostgreSQL setup
# =============================================================================
echo "--- PostgreSQL setup ---"
postgresql-setup --initdb || true
systemctl enable postgresql
systemctl start postgresql

# Wait for PostgreSQL to be ready
for i in $(seq 1 30); do
  pg_isready -U postgres && break
  echo "Waiting for PostgreSQL ($i/30)..."
  sleep 2
done

# Create user and database
sudo -u postgres psql -c "CREATE USER sme WITH PASSWORD '${DB_PASSWORD}';" 2>/dev/null || \
  sudo -u postgres psql -c "ALTER USER sme WITH PASSWORD '${DB_PASSWORD}';"

sudo -u postgres psql -c "CREATE DATABASE matching_engine OWNER sme;" 2>/dev/null || true

# Run schema migrations
echo "--- Running schema migrations ---"
sudo -u postgres psql -d matching_engine << 'SCHEMA'
-- Migration 001: Initial schema
CREATE TABLE IF NOT EXISTS pairs (
    id VARCHAR(20) PRIMARY KEY,
    base VARCHAR(10) NOT NULL,
    quote VARCHAR(10) NOT NULL,
    tick_size DECIMAL NOT NULL,
    lot_size DECIMAL NOT NULL,
    min_order_size DECIMAL NOT NULL,
    max_order_size DECIMAL NOT NULL,
    price_precision SMALLINT NOT NULL,
    qty_precision SMALLINT NOT NULL,
    price_band_pct DECIMAL NOT NULL DEFAULT 0.10,
    active BOOLEAN NOT NULL DEFAULT true
);

CREATE TABLE IF NOT EXISTS orders (
    id UUID PRIMARY KEY,
    user_id VARCHAR(50) NOT NULL,
    pair_id VARCHAR(20) NOT NULL REFERENCES pairs(id),
    side VARCHAR(4) NOT NULL,
    order_type VARCHAR(10) NOT NULL,
    tif VARCHAR(3) NOT NULL,
    price DECIMAL,
    quantity DECIMAL NOT NULL,
    remaining DECIMAL NOT NULL,
    status VARCHAR(20) NOT NULL,
    stp_mode VARCHAR(20) NOT NULL DEFAULT 'None',
    version BIGINT NOT NULL DEFAULT 1,
    sequence BIGSERIAL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_orders_pair_status ON orders(pair_id, status);
CREATE INDEX IF NOT EXISTS idx_orders_user ON orders(user_id, status);

CREATE TABLE IF NOT EXISTS trades (
    id UUID PRIMARY KEY,
    pair_id VARCHAR(20) NOT NULL REFERENCES pairs(id),
    buy_order_id UUID NOT NULL REFERENCES orders(id),
    sell_order_id UUID NOT NULL REFERENCES orders(id),
    buyer_id VARCHAR(50) NOT NULL,
    seller_id VARCHAR(50) NOT NULL,
    price DECIMAL NOT NULL,
    quantity DECIMAL NOT NULL,
    sequence BIGSERIAL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_trades_pair ON trades(pair_id, created_at DESC);

CREATE TABLE IF NOT EXISTS balances (
    user_id VARCHAR(50) NOT NULL,
    asset VARCHAR(10) NOT NULL,
    available DECIMAL NOT NULL DEFAULT 0,
    locked DECIMAL NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, asset)
);

CREATE TABLE IF NOT EXISTS audit_log (
    id BIGSERIAL PRIMARY KEY,
    sequence BIGINT NOT NULL,
    pair_id VARCHAR(20),
    event_type VARCHAR(30) NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_audit_pair ON audit_log(pair_id, created_at DESC);

-- Migration 003: Idempotency
ALTER TABLE orders ADD COLUMN IF NOT EXISTS client_order_id VARCHAR(64);
CREATE UNIQUE INDEX IF NOT EXISTS idx_orders_idempotency
  ON orders(user_id, client_order_id) WHERE client_order_id IS NOT NULL;
SCHEMA

# Seed data
sudo -u postgres psql -d matching_engine << 'SEED'
INSERT INTO pairs VALUES ('BTC-USDT','BTC','USDT',0.01,0.00001,0.00001,100,2,5,0.10,true) ON CONFLICT DO NOTHING;
INSERT INTO pairs VALUES ('ETH-USDT','ETH','USDT',0.01,0.0001,0.0001,1000,2,4,0.10,true) ON CONFLICT DO NOTHING;
INSERT INTO pairs VALUES ('SOL-USDT','SOL','USDT',0.001,0.01,0.01,10000,3,2,0.10,true) ON CONFLICT DO NOTHING;

INSERT INTO balances VALUES ('user-1','BTC',10,0) ON CONFLICT DO NOTHING;
INSERT INTO balances VALUES ('user-1','ETH',100,0) ON CONFLICT DO NOTHING;
INSERT INTO balances VALUES ('user-1','SOL',1000,0) ON CONFLICT DO NOTHING;
INSERT INTO balances VALUES ('user-1','USDT',1000000,0) ON CONFLICT DO NOTHING;
INSERT INTO balances VALUES ('user-2','BTC',10,0) ON CONFLICT DO NOTHING;
INSERT INTO balances VALUES ('user-2','ETH',100,0) ON CONFLICT DO NOTHING;
INSERT INTO balances VALUES ('user-2','SOL',1000,0) ON CONFLICT DO NOTHING;
INSERT INTO balances VALUES ('user-2','USDT',1000000,0) ON CONFLICT DO NOTHING;
SEED

# Allow password authentication from VPC (Lambda subnet range)
PG_HBA=$(find /var/lib/pgsql -name pg_hba.conf 2>/dev/null | head -1)
if [[ -n "${PG_HBA}" ]]; then
  # Allow sme user from VPC 10.0.0.0/8 with md5 auth
  grep -q "10.0.0.0/8" "${PG_HBA}" || \
    echo "host matching_engine sme 10.0.0.0/8 md5" >> "${PG_HBA}"
  # Also allow localhost
  grep -q "127.0.0.1/32.*sme" "${PG_HBA}" || \
    echo "host matching_engine sme 127.0.0.1/32 trust" >> "${PG_HBA}"
  systemctl reload postgresql
fi

echo "--- PostgreSQL ready ---"

# =============================================================================
# 3. Dragonfly setup
# =============================================================================
echo "--- Dragonfly setup ---"

# Download ARM (aarch64) build
DRAGONFLY_URL="https://github.com/dragonflydb/dragonfly/releases/download/v${DRAGONFLY_VERSION}/dragonfly-${ARCH}.tar.gz"
echo "Downloading Dragonfly ${DRAGONFLY_VERSION} for ${ARCH}..."
wget -q "${DRAGONFLY_URL}" -O /tmp/dragonfly.tar.gz

tar xzf /tmp/dragonfly.tar.gz -C /tmp
mv /tmp/dragonfly-${ARCH} /usr/local/bin/dragonfly
chmod +x /usr/local/bin/dragonfly
rm -f /tmp/dragonfly.tar.gz

# Create dragonfly system user and directories
useradd -r -s /bin/false dragonfly 2>/dev/null || true
mkdir -p /var/lib/dragonfly /etc/dragonfly
chown dragonfly:dragonfly /var/lib/dragonfly

# Dragonfly configuration
# --default_lua_flags=allow-undeclared-keys: REQUIRED by ADR-004
# Lua matching script uses undeclared keys in sorted sets
cat > /etc/dragonfly/dragonfly.conf << 'DRAGONCFG'
--bind=0.0.0.0
--port=6379
--maxmemory=256mb
--default_lua_flags=allow-undeclared-keys
--dir=/var/lib/dragonfly
--logtostderr
--loglevel=info
DRAGONCFG

# Dragonfly systemd service
cat > /etc/systemd/system/dragonfly.service << 'DFUNIT'
[Unit]
Description=Dragonfly In-Memory Data Store
Documentation=https://www.dragonflydb.io/docs
After=network.target
AssertFileIsExecutable=/usr/local/bin/dragonfly

[Service]
User=dragonfly
Group=dragonfly
WorkingDirectory=/var/lib/dragonfly
ExecStart=/usr/local/bin/dragonfly --flagfile=/etc/dragonfly/dragonfly.conf
Restart=always
RestartSec=5
LimitNOFILE=65536
LimitNPROC=65536

# Security hardening
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ReadWritePaths=/var/lib/dragonfly

[Install]
WantedBy=multi-user.target
DFUNIT

systemctl daemon-reload
systemctl enable dragonfly
systemctl start dragonfly

# Wait for Dragonfly to be ready
for i in $(seq 1 30); do
  redis-cli -p 6379 ping 2>/dev/null | grep -q PONG && break
  echo "Waiting for Dragonfly ($i/30)..."
  sleep 1
done

# Install redis-cli for health checks (from amazon-linux-extras or epel)
dnf install -y redis6 2>/dev/null || true

echo "--- Dragonfly ready ---"

# =============================================================================
# 4. sme-api worker binary
# =============================================================================
echo "--- sme-api setup ---"

if [[ -n "${SME_API_S3_URI}" ]]; then
  echo "Downloading sme-api from ${SME_API_S3_URI}..."
  aws s3 cp "${SME_API_S3_URI}" /usr/local/bin/sme-api
  chmod +x /usr/local/bin/sme-api
  echo "sme-api downloaded successfully"
else
  echo "WARNING: SME_API_S3_URI not set — sme-api will not be auto-installed."
  echo "To deploy manually:"
  echo "  1. Build: cargo build --release --target aarch64-unknown-linux-gnu --package sme-api"
  echo "  2. Copy:  scp target/aarch64-unknown-linux-gnu/release/sme-api ec2-user@<EC2_IP>:/usr/local/bin/"
  echo "  3. Start: sudo systemctl start sme-api"
  # Create a placeholder so the service file doesn't crash
  cat > /usr/local/bin/sme-api << 'PLACEHOLDER'
#!/bin/bash
echo "ERROR: sme-api binary not installed. Deploy via S3 or SCP."
exit 1
PLACEHOLDER
  chmod +x /usr/local/bin/sme-api
fi

# sme-api systemd service
# Workers MUST be stateless (ADR-001) — EC2 is fine; state lives in Dragonfly/PG
cat > /etc/systemd/system/sme-api.service << APIUNIT
[Unit]
Description=SME API Worker (BRPOP queue consumer + matching engine)
Documentation=https://github.com/ruzhukov-genai/serverless-matching-engine
After=network.target dragonfly.service postgresql.service
Requires=dragonfly.service

[Service]
User=ec2-user
Group=ec2-user
WorkingDirectory=/home/ec2-user

Environment="DATABASE_URL=postgres://sme:${DB_PASSWORD}@localhost:5432/matching_engine"
Environment="DRAGONFLY_URL=redis://localhost:6379"
Environment="RUST_LOG=info"

ExecStart=/usr/local/bin/sme-api
Restart=always
RestartSec=10
StartLimitInterval=60
StartLimitBurst=3

# Logging
StandardOutput=journal
StandardError=journal
SyslogIdentifier=sme-api

[Install]
WantedBy=multi-user.target
APIUNIT

systemctl daemon-reload
systemctl enable sme-api

# Only start if the binary is real (not the placeholder)
if grep -q "sme-api binary not installed" /usr/local/bin/sme-api 2>/dev/null; then
  echo "sme-api placeholder — NOT starting service. Deploy binary first."
else
  systemctl start sme-api
  echo "sme-api service started"
fi

# =============================================================================
# 5. CloudWatch Agent (optional, lightweight monitoring)
# =============================================================================
echo "--- CloudWatch Agent ---"
dnf install -y amazon-cloudwatch-agent 2>/dev/null || true

cat > /opt/aws/amazon-cloudwatch-agent/etc/amazon-cloudwatch-agent.json << 'CWCFG'
{
  "logs": {
    "logs_collected": {
      "files": {
        "collect_list": [
          {
            "file_path": "/var/log/sme-userdata.log",
            "log_group_name": "/sme/ec2/userdata",
            "log_stream_name": "{instance_id}"
          }
        ]
      },
      "systemd": {
        "collect_list": [
          {
            "log_group_name": "/sme/ec2/sme-api",
            "log_stream_name": "{instance_id}",
            "unit": "sme-api.service"
          },
          {
            "log_group_name": "/sme/ec2/dragonfly",
            "log_stream_name": "{instance_id}",
            "unit": "dragonfly.service"
          }
        ]
      }
    }
  }
}
CWCFG

systemctl enable amazon-cloudwatch-agent 2>/dev/null || true
systemctl start amazon-cloudwatch-agent 2>/dev/null || true

# =============================================================================
# 6. Summary
# =============================================================================
echo ""
echo "=== Bootstrap complete at $(date) ==="
echo ""
echo "Services:"
systemctl is-active postgresql && echo "  ✓ PostgreSQL: running" || echo "  ✗ PostgreSQL: FAILED"
systemctl is-active dragonfly  && echo "  ✓ Dragonfly:  running" || echo "  ✗ Dragonfly:  FAILED"
systemctl is-active sme-api    && echo "  ✓ sme-api:    running" || echo "  - sme-api:    not running (binary not deployed?)"
echo ""
echo "Next steps:"
echo "  - SSH: ssh -i <key>.pem ec2-user@<PUBLIC_IP>"
echo "  - Logs: journalctl -u sme-api -f"
echo "  - Deploy binary: scp target/aarch64-.../sme-api ec2-user@<IP>:/usr/local/bin/"
echo "  - Restart worker: sudo systemctl restart sme-api"
