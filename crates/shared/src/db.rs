//! PostgreSQL connection and migrations (sqlx built-in migrator).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::migrate::Migrator;
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

/// Run pending migrations from `./migrations/` directory.
/// Uses sqlx's built-in migrator which:
/// - Creates `_sqlx_migrations` table automatically
/// - Tracks applied migrations by version + checksum
/// - Only runs unapplied ones in sorted order
pub async fn run_migrations(pool: &PgPool) -> Result<()> {
    let migrator = Migrator::new(Path::new("./migrations")).await
        .context("load migrations")?;
    migrator.run(pool).await
        .context("run migrations")?;
    Ok(())
}
