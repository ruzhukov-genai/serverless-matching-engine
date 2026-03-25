use anyhow::Result;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use sme_shared::{cache, streams, Config, Trade};

const STREAM_TRANSACTIONS: &str = "stream:transactions";
const CONSUMER_GROUP: &str = "transaction-service-group";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    tracing::info!("transaction-service starting");

    let config = Config::from_env();
    let redis = cache::create_pool(&config.redis_url).await?;
    let pg = sme_shared::db::create_pool(&config.database_url).await?;
    sme_shared::db::run_migrations(&pg).await?;

    let worker_id = format!("ts-{}", Uuid::new_v4());
    streams::create_consumer_group(&redis, STREAM_TRANSACTIONS, CONSUMER_GROUP).await?;

    tracing::info!("entering consumer loop");

    loop {
        let messages =
            streams::consume(&redis, STREAM_TRANSACTIONS, CONSUMER_GROUP, &worker_id, 10)
                .await?;

        for msg in messages {
            let msg_id = msg.id.clone();

            let trade: Trade = match streams::deserialize(&msg) {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(error = %e, "failed to deserialize trade");
                    let _ =
                        streams::ack(&redis, STREAM_TRANSACTIONS, CONSUMER_GROUP, &msg_id)
                            .await;
                    continue;
                }
            };

            tracing::debug!(trade_id = %trade.id, pair = %trade.pair_id, "processing trade");

            // Insert trade (idempotent)
            if let Err(e) = insert_trade(&pg, &trade).await {
                tracing::error!(error = %e, "failed to insert trade");
                continue;
            }

            // Update balances
            if let Err(e) = settle_trade(&pg, &trade).await {
                tracing::error!(error = %e, "failed to settle trade balances");
                // Still ack so we don't loop forever — log for manual reconciliation
            }

            let _ =
                streams::ack(&redis, STREAM_TRANSACTIONS, CONSUMER_GROUP, &msg_id).await;
            tracing::info!(trade_id = %trade.id, "trade settled");
        }
    }
}

async fn insert_trade(pg: &sqlx::PgPool, trade: &Trade) -> Result<()> {
    sqlx::query(
        "INSERT INTO trades (id, pair_id, buy_order_id, sell_order_id, buyer_id, seller_id, price, quantity, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT DO NOTHING",
    )
    .bind(trade.id)
    .bind(&trade.pair_id)
    .bind(trade.buy_order_id)
    .bind(trade.sell_order_id)
    .bind(&trade.buyer_id)
    .bind(&trade.seller_id)
    .bind(trade.price)
    .bind(trade.quantity)
    .bind(trade.created_at)
    .execute(pg)
    .await?;
    Ok(())
}

async fn settle_trade(pg: &sqlx::PgPool, trade: &Trade) -> Result<()> {
    // Get pair base/quote assets
    let row = sqlx::query("SELECT base, quote FROM pairs WHERE id = $1")
        .bind(&trade.pair_id)
        .fetch_one(pg)
        .await?;

    use sqlx::Row;
    let base: String = row.try_get("base")?;
    let quote: String = row.try_get("quote")?;

    let cost = trade.price * trade.quantity; // quote amount

    // Use a transaction for atomicity
    let mut tx = pg.begin().await?;

    // Buyer: debit locked quote, credit base
    sqlx::query(
        "UPDATE balances SET locked = locked - $1 WHERE user_id = $2 AND asset = $3",
    )
    .bind(cost)
    .bind(&trade.buyer_id)
    .bind(&quote)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO balances (user_id, asset, available, locked) VALUES ($1, $2, $3, 0)
         ON CONFLICT (user_id, asset) DO UPDATE SET available = balances.available + $3",
    )
    .bind(&trade.buyer_id)
    .bind(&base)
    .bind(trade.quantity)
    .execute(&mut *tx)
    .await?;

    // Seller: debit locked base, credit quote
    sqlx::query(
        "UPDATE balances SET locked = locked - $1 WHERE user_id = $2 AND asset = $3",
    )
    .bind(trade.quantity)
    .bind(&trade.seller_id)
    .bind(&base)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO balances (user_id, asset, available, locked) VALUES ($1, $2, $3, 0)
         ON CONFLICT (user_id, asset) DO UPDATE SET available = balances.available + $3",
    )
    .bind(&trade.seller_id)
    .bind(&quote)
    .bind(cost)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}
