//! PostgreSQL connection and migrations.

use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

pub async fn create_pool(url: &str) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(50)
        .min_connections(5)
        .acquire_timeout(Duration::from_secs(3))
        .idle_timeout(Duration::from_secs(600))
        .max_lifetime(Duration::from_secs(1800))
        .connect(url)
        .await
        .context("connect to postgres")
}

pub async fn run_migrations(pool: &PgPool) -> Result<()> {
    // Execute each statement individually since sqlx doesn't support multi-statement raw_sql in prepared mode
    let statements = [
        "CREATE TABLE IF NOT EXISTS pairs (
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
        )",
        "CREATE TABLE IF NOT EXISTS orders (
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
        )",
        "CREATE INDEX IF NOT EXISTS idx_orders_pair_status ON orders(pair_id, status)",
        "CREATE INDEX IF NOT EXISTS idx_orders_user ON orders(user_id, status)",
        "CREATE TABLE IF NOT EXISTS trades (
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
        )",
        "CREATE INDEX IF NOT EXISTS idx_trades_pair ON trades(pair_id, created_at DESC)",
        "CREATE TABLE IF NOT EXISTS balances (
            user_id VARCHAR(50) NOT NULL,
            asset VARCHAR(10) NOT NULL,
            available DECIMAL NOT NULL DEFAULT 0,
            locked DECIMAL NOT NULL DEFAULT 0,
            PRIMARY KEY (user_id, asset)
        )",
        "CREATE TABLE IF NOT EXISTS audit_log (
            id BIGSERIAL PRIMARY KEY,
            sequence BIGINT NOT NULL,
            pair_id VARCHAR(20),
            event_type VARCHAR(30) NOT NULL,
            payload JSONB NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
        "CREATE INDEX IF NOT EXISTS idx_audit_pair ON audit_log(pair_id, created_at DESC)",
        // Migration 003: Idempotency
        "ALTER TABLE orders ADD COLUMN IF NOT EXISTS client_order_id VARCHAR(64)",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_orders_idempotency ON orders(user_id, client_order_id) WHERE client_order_id IS NOT NULL",
        // Migration 004: Performance indexes
        "CREATE INDEX IF NOT EXISTS idx_orders_user_status_created ON orders(user_id, status, created_at DESC)",
    ];

    for stmt in statements {
        sqlx::query(stmt)
            .execute(pool)
            .await
            .context("run migration statement")?;
    }

    Ok(())
}
