use anyhow::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    tracing::info!("transaction-service starting");

    // TODO:
    // 1. Connect to Dragonfly + PostgreSQL
    // 2. XREADGROUP stream:transactions
    // 3. Persist trades, update balances

    Ok(())
}
