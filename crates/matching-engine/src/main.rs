use anyhow::Result;
use tracing_subscriber::EnvFilter;

mod engine;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    tracing::info!("matching-engine starting");

    // TODO:
    // 1. Connect to Dragonfly (deadpool-redis)
    // 2. Connect to PostgreSQL (sqlx)
    // 3. Start consumer loop: XREADGROUP stream:orders:match
    // 4. For each order: lock -> load -> match -> write -> release

    Ok(())
}
