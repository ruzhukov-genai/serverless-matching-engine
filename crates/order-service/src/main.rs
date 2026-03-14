use anyhow::Result;
use rust_decimal::Decimal;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use sme_shared::{cache, streams, Config, Order, Side};

const STREAM_ORDERS: &str = "stream:orders";
const STREAM_ORDERS_MATCH: &str = "stream:orders:match";
const CONSUMER_GROUP: &str = "order-service-group";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    tracing::info!("order-service starting");

    let config = Config::from_env();
    let dragonfly = cache::create_pool(&config.dragonfly_url).await?;
    let pg = sme_shared::db::create_pool(&config.database_url).await?;
    sme_shared::db::run_migrations(&pg).await?;

    let worker_id = format!("os-{}", Uuid::new_v4());
    streams::create_consumer_group(&dragonfly, STREAM_ORDERS, CONSUMER_GROUP).await?;

    tracing::info!("entering consumer loop");

    loop {
        let messages =
            streams::consume(&dragonfly, STREAM_ORDERS, CONSUMER_GROUP, &worker_id, 10).await?;

        for msg in messages {
            let msg_id = msg.id.clone();

            let order: Order = match streams::deserialize(&msg) {
                Ok(o) => o,
                Err(e) => {
                    tracing::error!(error = %e, "failed to deserialize order from stream");
                    let _ = streams::ack(&dragonfly, STREAM_ORDERS, CONSUMER_GROUP, &msg_id).await;
                    continue;
                }
            };

            tracing::debug!(order_id = %order.id, pair = %order.pair_id, "validating order");

            // Validate order
            match validate_order(&pg, &order).await {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!(order_id = %order.id, reason = %e, "order rejected");
                    let _ = streams::ack(&dragonfly, STREAM_ORDERS, CONSUMER_GROUP, &msg_id).await;
                    continue;
                }
            }

            // Insert into DB
            if let Err(e) = insert_order_db(&pg, &order).await {
                tracing::error!(error = %e, "failed to insert order to db");
                continue;
            }

            // Lock balance
            if let Err(e) = lock_balance(&pg, &order).await {
                tracing::error!(error = %e, "failed to lock balance");
                continue;
            }

            // Save to Dragonfly cache
            if let Err(e) = cache::save_order_to_book(&dragonfly, &order).await {
                tracing::error!(error = %e, "failed to save order to cache");
            }

            // Publish to matching stream
            if let Err(e) = streams::publish(&dragonfly, STREAM_ORDERS_MATCH, &order).await {
                tracing::error!(error = %e, "failed to publish order to match stream");
            }

            let _ = streams::ack(&dragonfly, STREAM_ORDERS, CONSUMER_GROUP, &msg_id).await;
            tracing::info!(order_id = %order.id, "order accepted and queued for matching");
        }
    }
}

async fn validate_order(pg: &sqlx::PgPool, order: &Order) -> Result<()> {
    // Check pair exists and is active
    let row = sqlx::query("SELECT tick_size, lot_size, min_order_size, max_order_size, price_band_pct, active FROM pairs WHERE id = $1")
        .bind(&order.pair_id)
        .fetch_optional(pg)
        .await?;

    let row = match row {
        Some(r) => r,
        None => anyhow::bail!("unknown pair {}", order.pair_id),
    };

    use sqlx::Row;
    let active: bool = row.try_get("active")?;
    if !active {
        anyhow::bail!("pair {} is not active", order.pair_id);
    }

    let tick_size: Decimal = row.try_get("tick_size")?;
    let lot_size: Decimal = row.try_get("lot_size")?;
    let min_size: Decimal = row.try_get("min_order_size")?;
    let max_size: Decimal = row.try_get("max_order_size")?;

    // Lot alignment
    if order.quantity % lot_size != Decimal::ZERO {
        anyhow::bail!("quantity not aligned to lot_size {lot_size}");
    }
    if order.quantity < min_size {
        anyhow::bail!("quantity below min_order_size {min_size}");
    }
    if order.quantity > max_size {
        anyhow::bail!("quantity above max_order_size {max_size}");
    }

    // Tick alignment for limit orders
    if let Some(price) = order.price
        && tick_size > Decimal::ZERO
        && price % tick_size != Decimal::ZERO
    {
        anyhow::bail!("price not aligned to tick_size {tick_size}");
    }

    Ok(())
}

async fn insert_order_db(pg: &sqlx::PgPool, order: &Order) -> Result<()> {
    let status_str = format!("{:?}", order.status);
    let side_str = format!("{:?}", order.side);
    let type_str = format!("{:?}", order.order_type);
    let tif_str = format!("{:?}", order.tif);
    let stp_str = format!("{:?}", order.stp_mode);

    sqlx::query(
        "INSERT INTO orders (id, user_id, pair_id, side, order_type, tif, price, quantity, remaining, status, stp_mode, version, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
         ON CONFLICT DO NOTHING",
    )
    .bind(order.id)
    .bind(&order.user_id)
    .bind(&order.pair_id)
    .bind(&side_str)
    .bind(&type_str)
    .bind(&tif_str)
    .bind(order.price)
    .bind(order.quantity)
    .bind(order.remaining)
    .bind(&status_str)
    .bind(&stp_str)
    .bind(order.version)
    .bind(order.created_at)
    .bind(order.updated_at)
    .execute(pg)
    .await?;
    Ok(())
}

async fn lock_balance(pg: &sqlx::PgPool, order: &Order) -> Result<()> {
    // Determine what to lock: BUY locks quote, SELL locks base
    let (asset, lock_amount) = match order.side {
        Side::Buy => {
            let price = order.price.unwrap_or(Decimal::ZERO);
            let amount = price * order.quantity;
            // Get pair to find quote asset
            let row = sqlx::query("SELECT quote FROM pairs WHERE id = $1")
                .bind(&order.pair_id)
                .fetch_one(pg)
                .await?;
            use sqlx::Row;
            let quote: String = row.try_get("quote")?;
            (quote, amount)
        }
        Side::Sell => {
            let row = sqlx::query("SELECT base FROM pairs WHERE id = $1")
                .bind(&order.pair_id)
                .fetch_one(pg)
                .await?;
            use sqlx::Row;
            let base: String = row.try_get("base")?;
            (base, order.quantity)
        }
    };

    let rows_affected = sqlx::query(
        "UPDATE balances SET available = available - $1, locked = locked + $1
         WHERE user_id = $2 AND asset = $3 AND available >= $1",
    )
    .bind(lock_amount)
    .bind(&order.user_id)
    .bind(&asset)
    .execute(pg)
    .await?
    .rows_affected();

    if rows_affected == 0 {
        anyhow::bail!("insufficient balance for user {} asset {}", order.user_id, asset);
    }
    Ok(())
}
