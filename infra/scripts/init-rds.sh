#!/bin/bash
# Initialize RDS PostgreSQL schema and seed data.
# Run from EC2 instance (has network access to RDS).
#
# Usage: ./init-rds.sh <rds-endpoint> <db-password>
#   e.g.: ./init-rds.sh serverless-matching-engine-pg.xxx.us-east-1.rds.amazonaws.com sme_prod_xxx

set -euo pipefail

RDS_HOST="${1:?Usage: $0 <rds-endpoint> <db-password>}"
DB_PASSWORD="${2:?Usage: $0 <rds-endpoint> <db-password>}"

# Install postgresql client if not present
which psql > /dev/null 2>&1 || dnf install -y postgresql 2>/dev/null || true

export PGPASSWORD="$DB_PASSWORD"

echo "=== Creating schema on $RDS_HOST ==="

psql -h "$RDS_HOST" -U sme -d matching_engine << 'SQL'
CREATE TABLE IF NOT EXISTS pairs (
  id VARCHAR(20) PRIMARY KEY, base VARCHAR(10) NOT NULL, quote VARCHAR(10) NOT NULL,
  tick_size DECIMAL NOT NULL, lot_size DECIMAL NOT NULL, min_order_size DECIMAL NOT NULL,
  max_order_size DECIMAL NOT NULL, price_precision SMALLINT NOT NULL,
  qty_precision SMALLINT NOT NULL, price_band_pct DECIMAL NOT NULL DEFAULT 0.10,
  active BOOLEAN NOT NULL DEFAULT true
);
CREATE TABLE IF NOT EXISTS orders (
  id UUID PRIMARY KEY, user_id VARCHAR(50) NOT NULL, pair_id VARCHAR(20) NOT NULL REFERENCES pairs(id),
  side VARCHAR(4) NOT NULL, order_type VARCHAR(10) NOT NULL, tif VARCHAR(3) NOT NULL,
  price DECIMAL, quantity DECIMAL NOT NULL, remaining DECIMAL NOT NULL,
  status VARCHAR(20) NOT NULL, stp_mode VARCHAR(20) NOT NULL DEFAULT 'None',
  version BIGINT NOT NULL DEFAULT 1, sequence BIGSERIAL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  client_order_id VARCHAR(64)
);
CREATE INDEX IF NOT EXISTS idx_orders_pair_status ON orders(pair_id, status);
CREATE INDEX IF NOT EXISTS idx_orders_user ON orders(user_id, status);
CREATE UNIQUE INDEX IF NOT EXISTS idx_orders_idempotency ON orders(user_id, client_order_id) WHERE client_order_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS trades (
  id UUID PRIMARY KEY, pair_id VARCHAR(20) NOT NULL REFERENCES pairs(id),
  buy_order_id UUID NOT NULL REFERENCES orders(id), sell_order_id UUID NOT NULL REFERENCES orders(id),
  buyer_id VARCHAR(50) NOT NULL, seller_id VARCHAR(50) NOT NULL,
  price DECIMAL NOT NULL, quantity DECIMAL NOT NULL, sequence BIGSERIAL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_trades_pair ON trades(pair_id, created_at DESC);
CREATE TABLE IF NOT EXISTS balances (
  user_id VARCHAR(50) NOT NULL, asset VARCHAR(10) NOT NULL,
  available DECIMAL NOT NULL DEFAULT 0, locked DECIMAL NOT NULL DEFAULT 0,
  PRIMARY KEY (user_id, asset)
);
CREATE TABLE IF NOT EXISTS audit_log (
  id BIGSERIAL PRIMARY KEY, sequence BIGINT NOT NULL, pair_id VARCHAR(20),
  event_type VARCHAR(30) NOT NULL, payload JSONB NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_audit_pair ON audit_log(pair_id, created_at DESC);

-- Seed pairs
INSERT INTO pairs VALUES ('BTC-USDT','BTC','USDT',0.01,0.00001,0.00001,100,2,5,0.10,true) ON CONFLICT DO NOTHING;
INSERT INTO pairs VALUES ('ETH-USDT','ETH','USDT',0.01,0.0001,0.0001,1000,2,4,0.10,true) ON CONFLICT DO NOTHING;
INSERT INTO pairs VALUES ('SOL-USDT','SOL','USDT',0.001,0.01,0.01,10000,3,2,0.10,true) ON CONFLICT DO NOTHING;

-- Seed test balances
INSERT INTO balances VALUES ('user-1','BTC',10,0) ON CONFLICT DO NOTHING;
INSERT INTO balances VALUES ('user-1','ETH',100,0) ON CONFLICT DO NOTHING;
INSERT INTO balances VALUES ('user-1','SOL',1000,0) ON CONFLICT DO NOTHING;
INSERT INTO balances VALUES ('user-1','USDT',1000000,0) ON CONFLICT DO NOTHING;
INSERT INTO balances VALUES ('user-2','BTC',10,0) ON CONFLICT DO NOTHING;
INSERT INTO balances VALUES ('user-2','ETH',100,0) ON CONFLICT DO NOTHING;
INSERT INTO balances VALUES ('user-2','SOL',1000,0) ON CONFLICT DO NOTHING;
INSERT INTO balances VALUES ('user-2','USDT',1000000,0) ON CONFLICT DO NOTHING;
SQL

echo "=== Schema + seed data applied ==="
