use anyhow::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    tracing::info!("order-service starting");

    // TODO:
    // 1. Connect to Dragonfly + PostgreSQL
    // 2. Accept order requests (create, cancel)
    // 3. Validate, persist to DB
    // 4. XADD to stream:orders:match

    Ok(())
}
