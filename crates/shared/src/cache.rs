//! Order Book cache operations — sorted sets for bids/asks.
//!
//! Keys:
//!   book:{pair_id}:bids  → ZSet (score = -price, so ZRANGEBYSCORE -inf +inf = highest bid first)
//!   book:{pair_id}:asks  → ZSet (score = +price, so ZRANGEBYSCORE -inf +inf = lowest ask first)
//!   order:{order_id}     → Hash (field "data" = JSON-serialized Order)

use anyhow::{Context, Result};
use deadpool_redis::{Config as DPConfig, Pool, Runtime};
use redis::AsyncCommands;
use uuid::Uuid;

use crate::types::{Order, Side};

// ── Pool ─────────────────────────────────────────────────────────────────────

pub async fn create_pool(url: &str) -> Result<Pool> {
    let cfg = DPConfig::from_url(url);
    let pool = cfg
        .create_pool(Some(Runtime::Tokio1))
        .context("failed to create deadpool-redis pool")?;
    Ok(pool)
}

// ── Health ────────────────────────────────────────────────────────────────────

pub async fn health_check(pool: &Pool) -> Result<()> {
    let mut conn = pool.get().await.context("pool.get")?;
    let pong: String = redis::cmd("PING")
        .query_async(&mut *conn)
        .await
        .context("PING failed")?;
    if pong != "PONG" {
        anyhow::bail!("unexpected PING response: {pong}");
    }
    Ok(())
}

// ── Key helpers ───────────────────────────────────────────────────────────────

fn book_key(pair_id: &str, side: Side) -> String {
    match side {
        Side::Buy => format!("book:{pair_id}:bids"),
        Side::Sell => format!("book:{pair_id}:asks"),
    }
}

fn order_key(order_id: &Uuid) -> String {
    format!("order:{order_id}")
}

/// Score for the sorted set.  
/// Bids: negate price so ZRANGEBYSCORE -inf +inf returns highest bid first.  
/// Asks: raw price so ZRANGEBYSCORE -inf +inf returns lowest ask first.
fn book_score(order: &Order) -> f64 {
    let price = order
        .price
        .unwrap_or_default()
        .to_string()
        .parse::<f64>()
        .unwrap_or(0.0);
    match order.side {
        Side::Buy => -price,
        Side::Sell => price,
    }
}

// ── Order Book ────────────────────────────────────────────────────────────────

/// Load all orders for one side, sorted by price-time priority.
/// For Buy (bids): highest price first (ZRANGEBYSCORE -inf +inf on negated scores).
/// For Sell (asks): lowest price first.
pub async fn load_order_book(pool: &Pool, pair_id: &str, side: Side) -> Result<Vec<Order>> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = book_key(pair_id, side);

    // Returns members (order_id strings) in score order
    let ids: Vec<String> = redis::cmd("ZRANGEBYSCORE")
        .arg(&key)
        .arg("-inf")
        .arg("+inf")
        .query_async(&mut *conn)
        .await
        .context("ZRANGEBYSCORE")?;

    let mut orders = Vec::with_capacity(ids.len());
    for id in &ids {
        let okey = format!("order:{id}");
        let data: Option<String> = conn.hget(&okey, "data").await.context("HGET")?;
        if let Some(json) = data {
            match serde_json::from_str::<Order>(&json) {
                Ok(order) => orders.push(order),
                Err(e) => tracing::warn!("bad order json for {id}: {e}"),
            }
        }
    }
    Ok(orders)
}

/// Save an order to the sorted set + hash.
pub async fn save_order_to_book(pool: &Pool, order: &Order) -> Result<()> {
    let mut conn = pool.get().await.context("pool.get")?;
    let score = book_score(order);
    let zkey = book_key(&order.pair_id, order.side);
    let okey = order_key(&order.id);
    let json = serde_json::to_string(order).context("serialize order")?;

    // ZADD + HSET in pipeline
    let () = redis::pipe()
        .cmd("ZADD")
        .arg(&zkey)
        .arg(score)
        .arg(order.id.to_string())
        .cmd("HSET")
        .arg(&okey)
        .arg("data")
        .arg(&json)
        .query_async(&mut *conn)
        .await
        .context("ZADD + HSET")?;
    Ok(())
}

/// Remove an order from the sorted set + delete its hash.
pub async fn remove_order_from_book(pool: &Pool, order: &Order) -> Result<()> {
    let mut conn = pool.get().await.context("pool.get")?;
    let zkey = book_key(&order.pair_id, order.side);
    let okey = order_key(&order.id);
    let id_str = order.id.to_string();

    let () = redis::pipe()
        .cmd("ZREM")
        .arg(&zkey)
        .arg(&id_str)
        .cmd("DEL")
        .arg(&okey)
        .query_async(&mut *conn)
        .await
        .context("ZREM + DEL")?;
    Ok(())
}

/// Get a single order by id.
pub async fn get_order(pool: &Pool, order_id: &Uuid) -> Result<Option<Order>> {
    let mut conn = pool.get().await.context("pool.get")?;
    let okey = order_key(order_id);
    let data: Option<String> = conn.hget(&okey, "data").await.context("HGET")?;
    Ok(match data {
        Some(json) => Some(serde_json::from_str(&json).context("deserialize order")?),
        None => None,
    })
}

/// Atomically increment a per-pair version counter. Returns the new value.
pub async fn increment_version(pool: &Pool, pair_id: &str) -> Result<i64> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = format!("version:{pair_id}");
    let v: i64 = redis::cmd("INCR")
        .arg(&key)
        .query_async(&mut *conn)
        .await
        .context("INCR")?;
    Ok(v)
}

/// Read the current version counter without modifying it.
pub async fn get_version(pool: &Pool, pair_id: &str) -> Result<i64> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = format!("version:{pair_id}");
    let v: Option<i64> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut *conn)
        .await
        .context("GET version")?;
    Ok(v.unwrap_or(0))
}
