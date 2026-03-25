use anyhow::Result;
use chrono::Utc;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use sme_shared::{
    cache,
    engine::match_order,
    lock, streams,
    Config, Order, OrderStatus, Side, Trade,
};

mod engine;

const STREAM_ORDERS_MATCH: &str = "stream:orders:match";
const CONSUMER_GROUP: &str = "matching-engine-group";
const STREAM_TRANSACTIONS: &str = "stream:transactions";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    tracing::info!("matching-engine starting");

    let config = Config::from_env();
    let redis = cache::create_pool(&config.redis_url).await?;
    let pg = sme_shared::db::create_pool(&config.database_url).await?;
    sme_shared::db::run_migrations(&pg).await?;

    let worker_id = format!("me-{}", Uuid::new_v4());
    tracing::info!(worker_id = %worker_id, "worker id assigned");

    // Create consumer group (idempotent)
    streams::create_consumer_group(&redis, STREAM_ORDERS_MATCH, CONSUMER_GROUP).await?;

    tracing::info!("entering consumer loop");

    loop {
        let messages =
            streams::consume(&redis, STREAM_ORDERS_MATCH, CONSUMER_GROUP, &worker_id, 10)
                .await?;

        for msg in messages {
            let msg_id = msg.id.clone();

            let order: Order = match streams::deserialize(&msg) {
                Ok(o) => o,
                Err(e) => {
                    tracing::error!(msg_id = %msg_id, error = %e, "failed to deserialize order");
                    let _ = streams::ack(&redis, STREAM_ORDERS_MATCH, CONSUMER_GROUP, &msg_id).await;
                    continue;
                }
            };

            tracing::debug!(order_id = %order.id, pair = %order.pair_id, "processing order");

            // Acquire distributed lock for this pair
            let lock_guard = match lock::acquire_lock(&redis, &order.pair_id, &worker_id).await
            {
                Ok(g) => g,
                Err(e) => {
                    tracing::error!(pair = %order.pair_id, error = %e, "failed to acquire lock");
                    continue; // retry later
                }
            };

            // Load opposite side of the book
            let opposite = order.side.opposite();
            let mut book = cache::load_order_book(&redis, &order.pair_id, opposite).await?;

            // Sort book for price-time priority
            book.sort_by(|a, b| {
                let score_a = sort_score(a);
                let score_b = sort_score(b);
                score_a
                    .partial_cmp(&score_b)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.created_at.cmp(&b.created_at))
            });

            let result = match_order(&order, &mut book);

            // Persist book updates to cache
            for updated in &result.book_updates {
                if updated.status == OrderStatus::Filled
                    || updated.status == OrderStatus::Cancelled
                {
                    let _ = cache::remove_order_from_book(&redis, updated).await;
                } else {
                    let _ = cache::save_order_to_book(&redis, updated).await;
                }
            }

            // Persist incoming order
            if result.incoming.status == OrderStatus::New
                || result.incoming.status == OrderStatus::PartiallyFilled
            {
                let _ = cache::save_order_to_book(&redis, &result.incoming).await;
            } else {
                let _ = cache::remove_order_from_book(&redis, &result.incoming).await;
            }

            // Persist to DB
            for trade in &result.trades {
                if let Err(e) = insert_trade(&pg, trade).await {
                    tracing::error!(error = %e, "failed to insert trade to db");
                }
            }
            if let Err(e) = update_order_in_db(&pg, &result.incoming).await {
                tracing::error!(error = %e, "failed to update incoming order in db");
            }
            for upd in &result.book_updates {
                if let Err(e) = update_order_in_db(&pg, upd).await {
                    tracing::error!(error = %e, "failed to update book order in db");
                }
            }

            // Publish trades to transaction stream
            for trade in &result.trades {
                if let Err(e) = streams::publish(&redis, STREAM_TRANSACTIONS, trade).await {
                    tracing::error!(error = %e, "failed to publish trade to stream");
                }
            }

            // Release lock explicitly
            lock_guard.release().await;

            // Acknowledge the stream message
            let _ = streams::ack(&redis, STREAM_ORDERS_MATCH, CONSUMER_GROUP, &msg_id).await;

            tracing::info!(
                order_id = %order.id,
                trades = result.trades.len(),
                "order processed"
            );
        }
    }
}

fn sort_score(order: &Order) -> f64 {
    match order.side {
        Side::Sell => order
            .price
            .unwrap_or_default()
            .to_string()
            .parse()
            .unwrap_or(f64::MAX),
        Side::Buy => -order
            .price
            .unwrap_or_default()
            .to_string()
            .parse::<f64>()
            .unwrap_or(f64::MAX),
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

async fn update_order_in_db(pg: &sqlx::PgPool, order: &Order) -> Result<()> {
    sqlx::query(
        "UPDATE orders SET remaining = $1, status = $2, version = version + 1, updated_at = $3 WHERE id = $4",
    )
    .bind(order.remaining)
    .bind(format!("{:?}", order.status))
    .bind(Utc::now())
    .bind(order.id)
    .execute(pg)
    .await?;
    Ok(())
}
