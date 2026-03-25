//! Order Book cache operations — sorted sets for bids/asks.
//!
//! Keys:
//!   book:{pair_id}:bids  → ZSet (score = -price → ZRANGEBYSCORE -inf +inf = highest bid first)
//!   book:{pair_id}:asks  → ZSet (score = +price → ZRANGEBYSCORE -inf +inf = lowest ask first)
//!   order:{order_id}     → Hash with individual string fields (Lua-readable)
//!   version:{pair_id}    → monotonic counter, incremented on every book mutation
//!
//! Hash fields for order:{id}:
//!   id          → UUID string
//!   pair_id     → e.g. "BTC-USDT"
//!   side        → "B" (Buy) or "S" (Sell)
//!   order_type  → "L" (Limit) or "M" (Market)
//!   tif         → "G" (GTC), "I" (IOC), or "F" (FOK)
//!   price_i     → price * SCALE as i64 string ("0" for market orders)
//!   qty_i       → quantity * SCALE as i64 string
//!   remaining_i → remaining * SCALE as i64 string
//!   status      → "N" (New), "PF" (PartiallyFilled), "F" (Filled), "C" (Cancelled)
//!   stp         → "N", "CM", "CT", or "CB"
//!   user_id     → string
//!   ts_ms       → created_at.timestamp_millis() as i64 string
//!   version     → i64 string
//!
//! Scale factor: 100_000_000 (10^8) for 8-decimal precision.

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use deadpool_redis::{Config as DPConfig, Pool, Runtime};
use deadpool_redis::redis::AsyncCommands;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json;
use uuid::Uuid;

use crate::types::{Order, OrderStatus, OrderType, SelfTradePreventionMode, Side, TimeInForce};

// ── Scale factor ──────────────────────────────────────────────────────────────

/// Fixed-point scale: 10^8 gives 8 decimal places of precision.
/// BTC quantities up to ~21M and prices up to ~$90M fit safely within i64.
/// Lua doubles (f64) can represent integers up to 2^53 ≈ 9e15 exactly;
/// real-world crypto values (qty ≤ 21M * 10^8 = 2.1e15, price ≤ 90M * 10^8 = 9e15)
/// are within this safe range.
const SCALE: i64 = 100_000_000;

pub fn decimal_to_i64(d: Decimal) -> i64 {
    (d * Decimal::from(SCALE)).to_i64().unwrap_or(0)
}

pub fn i64_to_decimal(i: i64) -> Decimal {
    Decimal::from(i) / Decimal::from(SCALE)
}

// ── Pool ─────────────────────────────────────────────────────────────────────

pub async fn create_pool(url: &str) -> Result<Pool> {
    create_pool_sized(url, 200).await
}

/// Create a Valkey connection pool with a specific max size.
/// Use smaller pools where fewer connections are needed (e.g. gateway: ~20).
pub async fn create_pool_sized(url: &str, max_size: usize) -> Result<Pool> {
    let pool = DPConfig::from_url(url)
        .builder()
        .context("failed to create redis pool builder")?
        .max_size(max_size)
        .wait_timeout(Some(Duration::from_secs(3)))
        .runtime(Runtime::Tokio1)
        .build()
        .context("failed to create deadpool-redis pool")?;
    Ok(pool)
}

// ── Pub/Sub cache update ──────────────────────────────────────────────────────

/// Channel name for cache update pub/sub notifications.
pub const CACHE_UPDATES_CHANNEL: &str = "cache_updates";

/// SET a cache key in Valkey AND PUBLISH the update for gateway subscribers.
/// Format: "key\nvalue" — simple, zero-allocation parse on the subscriber side.
pub async fn set_and_publish(pool: &Pool, key: &str, value: &str) -> Result<()> {
    let mut conn = pool.get().await.context("pool.get")?;
    conn.set::<_, _, ()>(key, value).await.context("SET")?;
    let msg = format!("{}\n{}", key, value);
    conn.publish::<_, _, ()>(CACHE_UPDATES_CHANNEL, &msg).await.context("PUBLISH")?;
    Ok(())
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

// ── Pre-computed per-pair key strings ────────────────────────────────────────

/// Holds the three static Valkey key strings for one trading pair.
/// Computed once at startup and stored in AppState — avoids 3 format! calls
/// on every order on the hot path.
#[derive(Clone, Debug)]
pub struct PairKeys {
    pub bids_key: String,
    pub asks_key: String,
    pub version_key: String,
}

impl PairKeys {
    pub fn new(pair_id: &str) -> Self {
        Self {
            bids_key:    format!("book:{pair_id}:bids"),
            asks_key:    format!("book:{pair_id}:asks"),
            version_key: format!("version:{pair_id}"),
        }
    }
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

// ── Field names for HMGET ─────────────────────────────────────────────────────

const ORDER_FIELDS: &[&str] = &[
    "id", "pair_id", "side", "order_type", "tif",
    "price_i", "qty_i", "remaining_i", "status", "stp",
    "user_id", "ts_ms", "version",
];

// ── Order serialization helpers ───────────────────────────────────────────────

fn side_char(side: Side) -> &'static str {
    match side { Side::Buy => "B", Side::Sell => "S" }
}

fn order_type_char(ot: OrderType) -> &'static str {
    match ot { OrderType::Limit => "L", OrderType::Market => "M" }
}

fn tif_char(tif: TimeInForce) -> &'static str {
    match tif { TimeInForce::GTC => "G", TimeInForce::IOC => "I", TimeInForce::FOK => "F" }
}

fn status_char(s: OrderStatus) -> &'static str {
    match s {
        OrderStatus::New => "N",
        OrderStatus::PartiallyFilled => "PF",
        OrderStatus::Filled => "F",
        OrderStatus::Cancelled | OrderStatus::Rejected => "C",
    }
}

fn stp_char(stp: SelfTradePreventionMode) -> &'static str {
    match stp {
        SelfTradePreventionMode::None => "N",
        SelfTradePreventionMode::CancelMaker => "CM",
        SelfTradePreventionMode::CancelTaker => "CT",
        SelfTradePreventionMode::CancelBoth => "CB",
    }
}

/// Parse an order from HMGET results (13 fields, fixed positions as per ORDER_FIELDS).
/// Returns None if the hash is empty or any required field is missing/invalid.
fn parse_order_from_hmget(values: &[Option<String>]) -> Option<Order> {
    if values.len() < 13 {
        return None;
    }
    // If id is nil, the hash doesn't exist
    let id_str = values[0].as_deref()?;
    if id_str.is_empty() {
        return None;
    }

    let id: Uuid = id_str.parse().ok()?;
    let pair_id = values[1].clone()?;

    let side = match values[2].as_deref()? {
        "B" => Side::Buy,
        "S" => Side::Sell,
        _ => return None,
    };

    let order_type = match values[3].as_deref()? {
        "L" => OrderType::Limit,
        "M" => OrderType::Market,
        _ => return None,
    };

    let tif = match values[4].as_deref()? {
        "G" => TimeInForce::GTC,
        "I" => TimeInForce::IOC,
        "F" => TimeInForce::FOK,
        _ => return None,
    };

    let price_i: i64 = values[5].as_deref().unwrap_or("0").parse().ok()?;
    let qty_i: i64 = values[6].as_deref()?.parse().ok()?;
    let remaining_i: i64 = values[7].as_deref()?.parse().ok()?;

    let status = match values[8].as_deref()? {
        "N" => OrderStatus::New,
        "PF" => OrderStatus::PartiallyFilled,
        "F" => OrderStatus::Filled,
        "C" => OrderStatus::Cancelled,
        _ => OrderStatus::New,
    };

    let stp_mode = match values[9].as_deref()? {
        "N" => SelfTradePreventionMode::None,
        "CM" => SelfTradePreventionMode::CancelMaker,
        "CT" => SelfTradePreventionMode::CancelTaker,
        "CB" => SelfTradePreventionMode::CancelBoth,
        _ => SelfTradePreventionMode::None,
    };

    let user_id = values[10].clone()?;
    let ts_ms: i64 = values[11].as_deref()?.parse().ok()?;
    let version: i64 = values[12].as_deref().unwrap_or("1").parse().unwrap_or(1);

    // Price: None for market orders or when price_i == 0 and type is Market
    let price = if order_type == OrderType::Market || price_i == 0 {
        None
    } else {
        Some(i64_to_decimal(price_i))
    };

    let ts_secs = ts_ms / 1000;
    let ts_nanos = ((ts_ms % 1000) * 1_000_000) as u32;
    let created_at = Utc
        .timestamp_opt(ts_secs, ts_nanos)
        .single()
        .unwrap_or_else(Utc::now);

    Some(Order {
        id,
        user_id,
        pair_id,
        side,
        order_type,
        tif,
        price,
        quantity: i64_to_decimal(qty_i),
        remaining: i64_to_decimal(remaining_i),
        status,
        stp_mode,
        version,
        sequence: 0,       // sequence is DB-only
        created_at,
        updated_at: created_at, // updated_at not stored in cache; use created_at
        client_order_id: None,  // not stored in cache
        received_at: None,
        matched_at: None,
        persisted_at: None,
    })
}

// ── Internal: fetch orders by IDs (pipelined HMGET) ──────────────────────────

async fn fetch_orders_by_ids(pool: &Pool, ids: &[String]) -> Result<Vec<Order>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut conn = pool.get().await.context("pool.get")?;

    // Pipeline N × HMGET — one per order ID
    let mut pipe = redis::pipe();
    for id in ids {
        let okey = format!("order:{id}");
        pipe.cmd("HMGET").arg(&okey).arg(ORDER_FIELDS);
    }

    let results: Vec<Vec<Option<String>>> = pipe
        .query_async(&mut *conn)
        .await
        .context("pipeline HMGET")?;

    let mut orders = Vec::with_capacity(ids.len());
    for (id, values) in ids.iter().zip(results.iter()) {
        match parse_order_from_hmget(values) {
            Some(order) => orders.push(order),
            None => tracing::warn!("missing/corrupt order hash for {id}"),
        }
    }

    Ok(orders)
}

// ── Order Book ────────────────────────────────────────────────────────────────

/// Load all orders for one side, sorted by price-time priority.
/// For Buy (bids): highest price first (negated scores, ascending = most-negative first).
/// For Sell (asks): lowest price first (positive scores, ascending = cheapest first).
pub async fn load_order_book(pool: &Pool, pair_id: &str, side: Side) -> Result<Vec<Order>> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = book_key(pair_id, side);

    let ids: Vec<String> = redis::cmd("ZRANGEBYSCORE")
        .arg(&key)
        .arg("-inf")
        .arg("+inf")
        .query_async(&mut *conn)
        .await
        .context("ZRANGEBYSCORE")?;

    fetch_orders_by_ids(pool, &ids).await
}

/// Save an order to the sorted set + hash (individual string fields).
pub async fn save_order_to_book(pool: &Pool, order: &Order) -> Result<()> {
    let bids_key = format!("book:{}:bids", order.pair_id);
    let asks_key = format!("book:{}:asks", order.pair_id);
    let order_key = format!("order:{}", order.id);
    let price_i = order.price.map(decimal_to_i64).unwrap_or(0);
    let score = match order.side { Side::Buy => -price_i, Side::Sell => price_i } as f64;
    let book_key = match order.side { Side::Buy => &bids_key, Side::Sell => &asks_key };

    let mut conn = pool.get().await.context("pool.get")?;
    let () = redis::pipe()
        .cmd("ZADD").arg(book_key.as_str()).arg(score).arg(order.id.to_string())
        .cmd("HSET").arg(&order_key)
            .arg("id").arg(order.id.to_string())
            .arg("pair_id").arg(&order.pair_id)
            .arg("side").arg(side_char(order.side))
            .arg("order_type").arg(order_type_char(order.order_type))
            .arg("tif").arg(tif_char(order.tif))
            .arg("price_i").arg(price_i.to_string())
            .arg("qty_i").arg(decimal_to_i64(order.quantity).to_string())
            .arg("remaining_i").arg(decimal_to_i64(order.remaining).to_string())
            .arg("status").arg(status_char(order.status))
            .arg("stp").arg(stp_char(order.stp_mode))
            .arg("user_id").arg(&order.user_id)
            .arg("ts_ms").arg(order.created_at.timestamp_millis().to_string())
            .arg("version").arg(order.version.to_string())
        .query_async(&mut *conn)
        .await
        .context("ZADD + HSET")?;
    Ok(())
}

/// Remove an order from the sorted set and delete its hash.
pub async fn remove_order_from_book(pool: &Pool, order: &Order) -> Result<()> {
    let mut conn = pool.get().await.context("pool.get")?;
    let zkey = book_key(&order.pair_id, order.side);
    let okey = order_key(&order.id);
    let id_str = order.id.to_string();

    let () = redis::pipe()
        .cmd("ZREM").arg(&zkey).arg(&id_str)
        .cmd("DEL").arg(&okey)
        .query_async(&mut *conn)
        .await
        .context("ZREM + DEL")?;
    Ok(())
}

/// Get a single order by ID using HMGET.
pub async fn get_order(pool: &Pool, order_id: &Uuid) -> Result<Option<Order>> {
    let mut conn = pool.get().await.context("pool.get")?;
    let okey = order_key(order_id);

    let values: Vec<Option<String>> = redis::cmd("HMGET")
        .arg(&okey)
        .arg(ORDER_FIELDS)
        .query_async(&mut *conn)
        .await
        .context("HMGET")?;

    // All nil → key doesn't exist
    if values.iter().all(|v| v.is_none()) {
        return Ok(None);
    }

    Ok(parse_order_from_hmget(&values))
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

/// Load orders from one side with optional price filter.
pub async fn load_order_book_filtered(
    pool: &Pool,
    pair_id: &str,
    side: Side,
    limit_price: Option<Decimal>,
) -> Result<Vec<Order>> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = book_key(pair_id, side);

    let (min_score, max_score) = match limit_price {
        None => ("-inf".to_string(), "+inf".to_string()),
        Some(price) => {
            let price_i = decimal_to_i64(price);
            match side {
                // Asks stored with score = +price_i; filter asks <= buyer's price
                Side::Sell => ("-inf".to_string(), (price_i as f64).to_string()),
                // Bids stored with score = -price_i; filter bids >= seller's price → score <= -price_i
                Side::Buy => ("-inf".to_string(), (-(price_i as f64)).to_string()),
            }
        }
    };

    let ids: Vec<String> = redis::cmd("ZRANGEBYSCORE")
        .arg(&key)
        .arg(&min_score)
        .arg(&max_score)
        .query_async(&mut *conn)
        .await
        .context("ZRANGEBYSCORE filtered")?;

    fetch_orders_by_ids(pool, &ids).await
}

/// Load a batch of orders from one side using ZRANGEBYSCORE with LIMIT.
pub async fn load_order_book_batched(
    pool: &Pool,
    pair_id: &str,
    side: Side,
    offset: isize,
    count: isize,
) -> Result<Vec<Order>> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = book_key(pair_id, side);

    let ids: Vec<String> = redis::cmd("ZRANGEBYSCORE")
        .arg(&key)
        .arg("-inf")
        .arg("+inf")
        .arg("LIMIT")
        .arg(offset)
        .arg(count)
        .query_async(&mut *conn)
        .await
        .context("ZRANGEBYSCORE batched")?;

    fetch_orders_by_ids(pool, &ids).await
}

// ── Balance locking via Valkey (replaces PG hot-path lock) ─────────────────

/// Lock balance in Valkey atomically via Lua EVAL.
/// Eliminates the TOCTOU race in the previous HGET + pipeline approach.
pub async fn lock_balance_redis(
    pool: &Pool,
    user_id: &str,
    asset: &str,
    amount_scaled: i64,
) -> Result<()> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = format!("balance:{}:{}", user_id, asset);

    let result: redis::RedisResult<String> = LOCK_BALANCE_SCRIPT
        .prepare_invoke()
        .key(&key)
        .arg(amount_scaled.to_string())
        .invoke_async(&mut *conn)
        .await;

    match result {
        Ok(_) => Ok(()),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("INSUFFICIENT_BALANCE") {
                anyhow::bail!("insufficient {} balance for user {}", asset, user_id);
            }
            Err(anyhow::anyhow!("lock_balance Lua error: {e}"))
        }
    }
}

/// Release locked balance back to available in Valkey.
pub async fn unlock_balance_redis(
    pool: &Pool,
    user_id: &str,
    asset: &str,
    amount_scaled: i64,
) -> Result<()> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = format!("balance:{}:{}", user_id, asset);
    redis::pipe()
        .cmd("HINCRBY").arg(&key).arg("available").arg(amount_scaled)
        .cmd("HINCRBY").arg(&key).arg("locked").arg(-amount_scaled)
        .query_async::<()>(&mut *conn)
        .await
        .context("unlock_balance pipeline")?;
    Ok(())
}

/// Initialize Valkey balance keys from PG (called at startup).
pub async fn init_balances_from_pg(pool: &Pool, pg: &sqlx::PgPool) -> Result<()> {
    use sqlx::Row;
    let rows = sqlx::query("SELECT user_id, asset, available, locked FROM balances")
        .fetch_all(pg)
        .await
        .context("load balances from PG")?;

    let mut conn = pool.get().await.context("pool.get")?;
    for row in &rows {
        let user_id: String = row.get("user_id");
        let asset: String = row.get("asset");
        let available: Decimal = row.get("available");
        let locked: Decimal = row.get("locked");
        let key = format!("balance:{}:{}", user_id, asset);
        let avail_scaled = decimal_to_i64(available);
        let locked_scaled = decimal_to_i64(locked);
        redis::cmd("HSET")
            .arg(&key)
            .arg("available")
            .arg(avail_scaled)
            .arg("locked")
            .arg(locked_scaled)
            .query_async::<()>(&mut *conn)
            .await
            .context("HSET balance")?;
    }
    tracing::info!(count = rows.len(), "initialized Valkey balance keys from PG");
    Ok(())
}

/// Seed the order book ZSETs from PG open orders (called at cold start / cache migration).
///
/// Queries PG for all GTC orders with status IN ('New', 'PartiallyFilled') and
/// rebuilds `book:{pair_id}:bids`, `book:{pair_id}:asks`, and `order:{id}` HASHes
/// in Valkey. Safe to call on a fresh (empty) cache — idempotent.
///
/// Call this after `init_balances_from_pg()` on cold start so the Lua matching
/// script finds resting orders when the cache backend is new (e.g. after switching
/// to a fresh ElastiCache instance).
pub async fn seed_orderbook_from_pg(pool: &Pool, pg: &sqlx::PgPool) -> Result<()> {
    use sqlx::Row;

    let rows = sqlx::query(
        "SELECT id, user_id, pair_id, side, order_type, tif, \
                price, quantity, remaining, status, stp_mode, \
                version, created_at \
         FROM orders \
         WHERE status IN ('New', 'PartiallyFilled') \
           AND tif = 'GTC' \
         ORDER BY created_at ASC",
    )
    .fetch_all(pg)
    .await
    .context("load resting orders from PG")?;

    let count = rows.len();
    if count == 0 {
        tracing::info!("seed_orderbook_from_pg: no resting orders found");
        return Ok(());
    }

    for row in &rows {
        let id: uuid::Uuid = row.get("id");
        let user_id: String = row.get("user_id");
        let pair_id: String = row.get("pair_id");
        let side_str: String = row.get("side");
        let order_type_str: String = row.get("order_type");
        let tif_str: String = row.get("tif");
        let price: Option<Decimal> = row.get("price");
        let quantity: Decimal = row.get("quantity");
        let remaining: Decimal = row.get("remaining");
        let status_str: String = row.get("status");
        let stp_str: String = row.get("stp_mode");
        let version: i64 = row.get("version");
        let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");

        let side = match side_str.as_str() {
            "Buy" => crate::types::Side::Buy,
            "Sell" => crate::types::Side::Sell,
            s => {
                tracing::warn!(order_id = %id, side = s, "seed_orderbook_from_pg: unknown side, skipping");
                continue;
            }
        };
        let order_type = match order_type_str.as_str() {
            "Limit" => crate::types::OrderType::Limit,
            "Market" => crate::types::OrderType::Market,
            s => {
                tracing::warn!(order_id = %id, order_type = s, "seed_orderbook_from_pg: unknown order_type, skipping");
                continue;
            }
        };
        let tif = match tif_str.as_str() {
            "GTC" => crate::types::TimeInForce::GTC,
            "IOC" => crate::types::TimeInForce::IOC,
            "FOK" => crate::types::TimeInForce::FOK,
            s => {
                tracing::warn!(order_id = %id, tif = s, "seed_orderbook_from_pg: unknown tif, skipping");
                continue;
            }
        };
        let status = match status_str.as_str() {
            "New" => crate::types::OrderStatus::New,
            "PartiallyFilled" => crate::types::OrderStatus::PartiallyFilled,
            _ => crate::types::OrderStatus::New,
        };
        let stp_mode = match stp_str.as_str() {
            "CancelMaker" => crate::types::SelfTradePreventionMode::CancelMaker,
            "CancelTaker" => crate::types::SelfTradePreventionMode::CancelTaker,
            "CancelBoth" => crate::types::SelfTradePreventionMode::CancelBoth,
            _ => crate::types::SelfTradePreventionMode::None,
        };

        let order = crate::types::Order {
            id,
            user_id,
            pair_id,
            side,
            order_type,
            tif,
            price,
            quantity,
            remaining,
            status,
            stp_mode,
            version: version as i64,
            sequence: 0,
            created_at,
            updated_at: created_at,
            client_order_id: None,
            received_at: None,
            matched_at: None,
            persisted_at: None,
        };

        save_order_to_book(pool, &order).await
            .with_context(|| format!("seed_orderbook_from_pg: save_order_to_book failed for {}", id))?;
    }

    tracing::info!(count = count, "seed_orderbook_from_pg: seeded resting orders into cache");
    Ok(())
}

/// Seed ticker cache from recent trades in PG (called at cold start / cache migration).
///
/// Populates `cache:ticker:{pair}` with last price, 24h high/low/volume derived from
/// the trades table. Safe to call on an empty cache — idempotent.
///
/// Call this alongside `seed_orderbook_from_pg()` on cold start so the Gateway
/// returns meaningful ticker data even before the first new trade arrives.
pub async fn seed_ticker_from_pg(
    pool: &Pool,
    pg: &sqlx::PgPool,
    pair_ids: &[String],
) -> Result<()> {
    use sqlx::Row;

    for pair_id in pair_ids {
        let ticker_row = sqlx::query(
            "SELECT \
                MAX(price) FILTER (WHERE created_at >= NOW() - INTERVAL '24 hours') as high_24h, \
                MIN(price) FILTER (WHERE created_at >= NOW() - INTERVAL '24 hours') as low_24h, \
                SUM(quantity) FILTER (WHERE created_at >= NOW() - INTERVAL '24 hours') as volume_24h \
             FROM trades WHERE pair_id = $1",
        )
        .bind(pair_id)
        .fetch_optional(pg)
        .await
        .context("seed_ticker_from_pg: ticker query failed")?;

        let last_row = sqlx::query(
            "SELECT price as last_price FROM trades WHERE pair_id = $1 \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(pair_id)
        .fetch_optional(pg)
        .await
        .context("seed_ticker_from_pg: last price query failed")?;

        let last = last_row
            .and_then(|r| r.get::<Option<Decimal>, _>("last_price"))
            .map(|v| v.to_string());

        let ticker_json = if let Some(row) = ticker_row {
            serde_json::json!({
                "pair": pair_id,
                "last": last,
                "high_24h": row.get::<Option<Decimal>, _>("high_24h").map(|v| v.to_string()),
                "low_24h": row.get::<Option<Decimal>, _>("low_24h").map(|v| v.to_string()),
                "volume_24h": row.get::<Option<Decimal>, _>("volume_24h").map(|v| v.to_string()),
            })
        } else {
            serde_json::json!({
                "pair": pair_id,
                "last": last,
                "high_24h": null,
                "low_24h": null,
                "volume_24h": null,
            })
        };

        let ticker_str = serde_json::to_string(&ticker_json)
            .context("seed_ticker_from_pg: serialize failed")?;
        set_and_publish(pool, &format!("cache:ticker:{}", pair_id), &ticker_str).await
            .with_context(|| format!("seed_ticker_from_pg: set_and_publish failed for {}", pair_id))?;
    }

    tracing::info!(count = pair_ids.len(), "seed_ticker_from_pg: ticker cache seeded from PG");
    Ok(())
}

// ── Orderbook snapshot via Lua (single round-trip) ────────────────────────────

/// Lua script that reads top N bids and asks, aggregates by price level,
/// and returns the result in a single EVAL — replacing 100+ round-trips.
const ORDERBOOK_SNAPSHOT_LUA: &str = r#"
local pair_id = KEYS[1]
local limit = tonumber(ARGV[1]) or 50

local function aggregate_side(book_key)
    local ids = redis.call('ZRANGEBYSCORE', book_key, '-inf', '+inf', 'LIMIT', 0, limit)
    local levels = {}
    local level_order = {}
    for _, id in ipairs(ids) do
        local pi = redis.call('HGET', 'order:' .. id, 'price_i')
        local ri = redis.call('HGET', 'order:' .. id, 'remaining_i')
        if pi and ri then
            local p = tostring(pi)
            local r = tonumber(ri) or 0
            if r > 0 then
                if levels[p] then
                    levels[p] = levels[p] + r
                else
                    levels[p] = r
                    level_order[#level_order + 1] = p
                end
            end
        end
    end
    local result = {}
    for _, p in ipairs(level_order) do
        result[#result + 1] = p
        result[#result + 1] = tostring(levels[p])
    end
    return result
end

local bids = aggregate_side('book:' .. pair_id .. ':bids')
local asks = aggregate_side('book:' .. pair_id .. ':asks')

-- Return: [bid_count, bid_levels..., ask_count, ask_levels...]
local result = {#bids}
for _, v in ipairs(bids) do result[#result + 1] = v end
result[#result + 1] = #asks
for _, v in ipairs(asks) do result[#result + 1] = v end
return result
"#;

// ── Cached Lua Script objects (SHA1 computed once at startup) ─────────────────

static MATCH_SCRIPT: LazyLock<redis::Script> =
    LazyLock::new(|| redis::Script::new(include_str!("lua/match_order.lua")));

static CAS_LUA_SCRIPT: LazyLock<redis::Script> =
    LazyLock::new(|| redis::Script::new(CAS_SCRIPT));

static SNAPSHOT_LUA_SCRIPT: LazyLock<redis::Script> =
    LazyLock::new(|| redis::Script::new(ORDERBOOK_SNAPSHOT_LUA));

// ── Atomic balance lock via Lua ───────────────────────────────────────────────

const LOCK_BALANCE_LUA: &str = r#"
local key = KEYS[1]
local amount = tonumber(ARGV[1])
local avail = tonumber(redis.call('HGET', key, 'available') or '0')
if avail == nil or avail < amount then
    return redis.error_reply('INSUFFICIENT_BALANCE')
end
redis.call('HINCRBY', key, 'available', -amount)
redis.call('HINCRBY', key, 'locked', amount)
return 'OK'
"#;

static LOCK_BALANCE_SCRIPT: LazyLock<redis::Script> =
    LazyLock::new(|| redis::Script::new(LOCK_BALANCE_LUA));

/// Load aggregated orderbook levels via a single Lua EVAL.
/// Returns (bids_agg, asks_agg) where each is Vec<(price_str, qty_str)>.
pub async fn orderbook_snapshot_lua(
    pool: &Pool,
    pair_id: &str,
    limit: usize,
) -> Result<(Vec<(String, String)>, Vec<(String, String)>)> {
    let mut conn = pool.get().await.context("pool.get")?;
    let result: Vec<String> = SNAPSHOT_LUA_SCRIPT.prepare_invoke()
        .key(pair_id)
        .arg(limit)
        .invoke_async(&mut *conn)
        .await
        .context("orderbook_snapshot_lua EVAL")?;

    // Parse: [bid_count, bid_price, bid_qty, ..., ask_count, ask_price, ask_qty, ...]
    let mut iter = result.into_iter();
    let bid_count: usize = iter.next().unwrap_or_default().parse().unwrap_or(0);

    let mut bids = Vec::with_capacity(bid_count / 2);
    for _ in 0..bid_count / 2 {
        if let (Some(p), Some(q)) = (iter.next(), iter.next()) {
            // Convert from i64 scale to decimal string
            let price = i64_to_decimal(p.parse::<i64>().unwrap_or(0).abs());
            let qty = i64_to_decimal(q.parse::<i64>().unwrap_or(0));
            bids.push((price.to_string(), qty.to_string()));
        }
    }

    let ask_count: usize = iter.next().unwrap_or_default().parse().unwrap_or(0);
    let mut asks = Vec::with_capacity(ask_count / 2);
    for _ in 0..ask_count / 2 {
        if let (Some(p), Some(q)) = (iter.next(), iter.next()) {
            let price = i64_to_decimal(p.parse::<i64>().unwrap_or(0));
            let qty = i64_to_decimal(q.parse::<i64>().unwrap_or(0));
            asks.push((price.to_string(), qty.to_string()));
        }
    }

    Ok((bids, asks))
}

// ── OCC / CAS primitives ──────────────────────────────────────────────────────

/// Result of a compare-and-swap attempt.
pub enum CasResult {
    /// Mutations applied atomically, version incremented.
    Ok,
    /// Version changed between snapshot and CAS — caller should retry.
    Conflict,
}

/// Load the order book snapshot AND the current version in a single pipeline.
/// Used by the OCC retry loop (FOK orders and cancel/modify operations).
///
/// Returns `(orders, version)` where `version` is the monotonic counter at
/// `version:{pair_id}`.
pub async fn load_order_book_snapshot(
    pool: &Pool,
    pair_id: &str,
    side: Side,
    limit_price: Option<Decimal>,
) -> Result<(Vec<Order>, i64)> {
    let mut conn = pool.get().await.context("pool.get")?;
    let zkey = book_key(pair_id, side);
    let version_key = format!("version:{pair_id}");

    let (min_score, max_score) = match limit_price {
        None => ("-inf".to_string(), "+inf".to_string()),
        Some(price) => {
            let price_i = decimal_to_i64(price);
            match side {
                // Asks stored with score = +price_i; filter asks <= buyer's price
                Side::Sell => ("-inf".to_string(), (price_i as f64).to_string()),
                // Bids stored with score = -price_i; filter bids >= seller's price → score <= -price_i
                Side::Buy => ("-inf".to_string(), (-(price_i as f64)).to_string()),
            }
        }
    };

    // Pipeline: GET version + ZRANGEBYSCORE
    let (version_raw, ids): (Option<String>, Vec<String>) = redis::pipe()
        .cmd("GET").arg(&version_key)
        .cmd("ZRANGEBYSCORE").arg(&zkey).arg(&min_score).arg(&max_score)
        .query_async(&mut *conn)
        .await
        .context("pipeline GET version + ZRANGEBYSCORE")?;

    let version: i64 = version_raw
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    if ids.is_empty() {
        return Ok((Vec::new(), version));
    }

    // Pipeline HMGET for all order IDs
    let mut hget_pipe = redis::pipe();
    for id in &ids {
        let okey = format!("order:{id}");
        hget_pipe.cmd("HMGET").arg(&okey).arg(ORDER_FIELDS);
    }

    let data_list: Vec<Vec<Option<String>>> = hget_pipe
        .query_async(&mut *conn)
        .await
        .context("pipeline HMGET orders")?;

    let mut orders = Vec::with_capacity(ids.len());
    for (id, values) in ids.iter().zip(data_list.iter()) {
        match parse_order_from_hmget(values) {
            Some(order) => orders.push(order),
            None => tracing::warn!("missing/corrupt order hash for {id}"),
        }
    }

    Ok((orders, version))
}

/// The Lua CAS script — atomically checks version, applies ZSet mutations, increments version.
///
/// All accessed keys are declared in KEYS[] (Valkey/Redis cluster compatibility).
/// Order hash data is pre-written by the caller (Phase 1) before this script runs (Phase 2).
///
/// KEYS: [1]=version, [2]=bids_zset, [3]=asks_zset, [4..4+N-1]=order hash keys (one per mutation)
/// ARGV: [1]=expected_version, [2]=N, then 3 fields per mutation: op, order_id, score
const CAS_SCRIPT: &str = r#"
local current = redis.call('GET', KEYS[1])
if tostring(current) ~= ARGV[1] then
    return redis.error_reply('VERSION_CONFLICT')
end

local n = tonumber(ARGV[2])
for i = 0, n-1 do
    local op    = ARGV[3 + i*3 + 0]
    local oid   = ARGV[3 + i*3 + 1]
    local score = ARGV[3 + i*3 + 2]
    local okey  = KEYS[4 + i]

    if op == 'REMOVE_BID' then
        redis.call('ZREM', KEYS[2], oid)
        redis.call('DEL', okey)
    elseif op == 'REMOVE_ASK' then
        redis.call('ZREM', KEYS[3], oid)
        redis.call('DEL', okey)
    elseif op == 'UPSERT_BID' then
        redis.call('ZADD', KEYS[2], score, oid)
        -- HSET data already written by caller before this script
    elseif op == 'UPSERT_ASK' then
        redis.call('ZADD', KEYS[3], score, oid)
        -- HSET data already written by caller before this script
    end
end

redis.call('INCR', KEYS[1])
return 'OK'
"#;

/// Apply book mutations atomically via Lua CAS.
///
/// Two-phase approach:
/// 1. Pre-write all HSET (order hash) data for UPSERT ops (pipeline, outside Lua).
/// 2. Lua CAS: check version, ZADD/ZREM, INCR version.
///
/// Used for FOK orders (which take the OCC path) and cancel/modify operations.
/// All other orders go through `match_order_lua` which handles mutations atomically in Lua.
pub async fn apply_book_mutations_cas(
    pool: &Pool,
    pair_id: &str,
    expected_version: i64,
    book_updates: &[Order],
    incoming_if_resting: Option<&Order>,
) -> Result<CasResult> {
    let version_key = format!("version:{pair_id}");
    let bids_key = format!("book:{pair_id}:bids");
    let asks_key = format!("book:{pair_id}:asks");

    struct Mutation {
        op: &'static str,
        oid: String,
        order_key: String,
        score: f64,
    }

    let mut mutations: Vec<Mutation> = Vec::new();
    let mut upsert_orders: Vec<&Order> = Vec::new();

    for upd in book_updates {
        let (op, needs_hset): (&'static str, bool) = match upd.side {
            Side::Buy => {
                if upd.status == OrderStatus::Filled || upd.status == OrderStatus::Cancelled {
                    ("REMOVE_BID", false)
                } else {
                    ("UPSERT_BID", true)
                }
            }
            Side::Sell => {
                if upd.status == OrderStatus::Filled || upd.status == OrderStatus::Cancelled {
                    ("REMOVE_ASK", false)
                } else {
                    ("UPSERT_ASK", true)
                }
            }
        };
        if needs_hset {
            upsert_orders.push(upd);
        }
        let price_i = upd.price.map(decimal_to_i64).unwrap_or(0);
        let score = match upd.side { Side::Buy => -price_i, Side::Sell => price_i } as f64;
        mutations.push(Mutation {
            op,
            oid: upd.id.to_string(),
            order_key: format!("order:{}", upd.id),
            score,
        });
    }

    if let Some(incoming) = incoming_if_resting {
        let op = match incoming.side {
            Side::Buy => "UPSERT_BID",
            Side::Sell => "UPSERT_ASK",
        };
        upsert_orders.push(incoming);
        let price_i = incoming.price.map(decimal_to_i64).unwrap_or(0);
        let score = match incoming.side { Side::Buy => -price_i, Side::Sell => price_i } as f64;
        mutations.push(Mutation {
            op,
            oid: incoming.id.to_string(),
            order_key: format!("order:{}", incoming.id),
            score,
        });
    }

    let mut conn = pool.get().await.context("pool.get")?;

    // Phase 1: Pre-write HSET data for all UPSERT ops (individual fields, not msgpack)
    if !upsert_orders.is_empty() {
        let mut hset_pipe = redis::pipe();
        for order in &upsert_orders {
            let okey = format!("order:{}", order.id);
            let price_i = order.price.map(decimal_to_i64).unwrap_or(0);
            hset_pipe.cmd("HSET").arg(&okey)
                .arg("id").arg(order.id.to_string())
                .arg("pair_id").arg(&order.pair_id)
                .arg("side").arg(side_char(order.side))
                .arg("order_type").arg(order_type_char(order.order_type))
                .arg("tif").arg(tif_char(order.tif))
                .arg("price_i").arg(price_i.to_string())
                .arg("qty_i").arg(decimal_to_i64(order.quantity).to_string())
                .arg("remaining_i").arg(decimal_to_i64(order.remaining).to_string())
                .arg("status").arg(status_char(order.status))
                .arg("stp").arg(stp_char(order.stp_mode))
                .arg("user_id").arg(&order.user_id)
                .arg("ts_ms").arg(order.created_at.timestamp_millis().to_string())
                .arg("version").arg(order.version.to_string())
                .ignore();
        }
        hset_pipe.query_async::<()>(&mut *conn).await.context("pre-write HSET pipeline")?;
    }

    // Phase 2: Lua CAS — version check + ZADD/ZREM + INCR
    let n = mutations.len();
    let mut invocation = CAS_LUA_SCRIPT.prepare_invoke();
    invocation
        .key(&version_key)
        .key(&bids_key)
        .key(&asks_key);

    for m in &mutations {
        invocation.key(&m.order_key);
    }

    invocation
        .arg(expected_version.to_string())
        .arg(n.to_string());

    for m in &mutations {
        invocation.arg(m.op).arg(&m.oid).arg(m.score.to_string());
    }

    let result: redis::RedisResult<String> = invocation.invoke_async(&mut *conn).await;

    match result {
        Ok(_) => Ok(CasResult::Ok),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("VERSION_CONFLICT") {
                Ok(CasResult::Conflict)
            } else {
                Err(anyhow::anyhow!("CAS script error: {e}"))
            }
        }
    }
}

// ── Lua Matching ──────────────────────────────────────────────────────────────

/// Result of `match_order_lua` — the atomic Lua match.
pub struct LuaMatchResult {
    pub remaining: Decimal,
    pub status: OrderStatus,
    pub trades: Vec<LuaTrade>,
}

/// A single trade produced by the Lua matching script.
pub struct LuaTrade {
    pub resting_order_id: Uuid,
    pub price: Decimal,
    pub quantity: Decimal,
    pub buyer_id: String,
    pub seller_id: String,
}

/// Extract a String from a redis::Value.
fn redis_value_to_string(v: &redis::Value) -> Option<String> {
    match v {
        redis::Value::BulkString(bytes) => String::from_utf8(bytes.clone()).ok(),
        redis::Value::SimpleString(s) => Some(s.clone()),
        redis::Value::Int(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Match an incoming order atomically inside Valkey via a single EVAL round-trip.
///
/// The Lua script calls ZRANGEBYSCORE internally to fetch resting order IDs,
/// then accesses their hashes directly. Valkey (non-cluster) allows undeclared
/// key access in scripts — no Phase 1 round-trip needed.
///
/// The balance lock is now merged into this EVAL (Opt 1) — saves 1 round-trip.
/// Pass `lock_asset` and `lock_amount_scaled` for the asset to lock; pass
/// `lock_amount_scaled = 0` to skip locking (e.g. market buy with unknown price).
///
/// `pair_keys` should come from AppState — pre-computed at startup (Opt 3).
///
/// Total: 1 round-trip (was 2), no lock, no retry, no CAS version check.
///
/// # FOK note
/// FOK orders are NOT handled here. See the OCC/CAS path in routes.rs.
pub async fn match_order_lua(
    pool: &Pool,
    order: &Order,
    pair_keys: &PairKeys,
    lock_asset: &str,
    lock_amount_scaled: i64,
) -> Result<LuaMatchResult> {
    let bids_key = &pair_keys.bids_key;
    let asks_key = &pair_keys.asks_key;
    let version_key = &pair_keys.version_key;
    let incoming_order_key = format!("order:{}", order.id);

    let price_i = order.price.map(decimal_to_i64).unwrap_or(0);
    let _score = order.book_score();

    // Calculate price bound (max_score) for ZRANGEBYSCORE inside Lua.
    // Ask scores are positive (+price_i). Bid scores are negative (-price_i).
    let max_score = match (order.side, order.price) {
        (Side::Buy, Some(price)) => {
            // Buying: match asks where ask_score <= our price_i
            let max = decimal_to_i64(price) as f64;
            max.to_string()
        }
        (Side::Sell, Some(price)) => {
            // Selling: match bids where bid_score <= -our price_i
            let max = -(decimal_to_i64(price) as f64);
            max.to_string()
        }
        _ => "+inf".to_string(), // Market orders — match everything
    };

    let mut conn = pool.get().await.context("pool.get")?;

    // Single Lua EVAL — balance lock + ZRANGEBYSCORE + matching all in one atomic call
    // KEYS: [1]=bids, [2]=asks, [3]=version, [4]=order:{incoming_id}
    // ARGV: [1..12]=order fields + max_score, [13]=lock_asset, [14]=lock_amount_scaled
    let mut invocation = MATCH_SCRIPT.prepare_invoke();

    invocation
        .key(bids_key.as_str())
        .key(asks_key.as_str())
        .key(version_key.as_str())
        .key(&incoming_order_key);

    invocation
        .arg(order.id.to_string())
        .arg(side_char(order.side))
        .arg(order_type_char(order.order_type))
        .arg(tif_char(order.tif))
        .arg(price_i.to_string())
        .arg(decimal_to_i64(order.quantity).to_string())
        .arg(&order.user_id)
        .arg(stp_char(order.stp_mode))
        .arg(order.created_at.timestamp_millis().to_string())
        .arg(&order.pair_id)
        // CRITICAL: Use scaled price_i as score so ZRANGEBYSCORE bounds match (see ADR-004)
        // Buy: score = -(price_i), Sell: score = +(price_i) → both are large scaled integers
        .arg((match order.side { Side::Buy => -price_i, Side::Sell => price_i }) as f64)
        .arg(&max_score)
        .arg(lock_asset)
        .arg(lock_amount_scaled.to_string());

    let raw: redis::Value = invocation
        .invoke_async(&mut *conn)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Lua match_order EVAL failed (redis error)");
            anyhow::anyhow!("Lua match_order EVAL: {e}")
        })?;

    // Parse the returned bulk array:
    //   [0] = "OK"  (or "INSUFFICIENT_BALANCE" for balance lock failure)
    //   [1] = remaining_i
    //   [2] = status
    //   [3] = trade_count
    //   then 5 fields per trade: resting_id, price_i, qty_i, buyer_id, seller_id
    let items = match raw {
        redis::Value::Array(v) => v,
        other => anyhow::bail!("unexpected Lua return type: {:?}", other),
    };

    // Check for INSUFFICIENT_BALANCE first (single-element array returned by Lua)
    if let Some(first) = items.first() {
        if redis_value_to_string(first).as_deref() == Some("INSUFFICIENT_BALANCE") {
            anyhow::bail!("INSUFFICIENT_BALANCE: insufficient {} for user {}", lock_asset, order.user_id);
        }
    }

    if items.len() < 4 {
        anyhow::bail!("Lua script returned too few fields: {}", items.len());
    }

    let ok_str = redis_value_to_string(&items[0]).unwrap_or_default();
    if ok_str != "OK" {
        anyhow::bail!("Lua script returned error: {ok_str}");
    }

    let remaining_i: i64 = redis_value_to_string(&items[1])
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let status_str = redis_value_to_string(&items[2]).unwrap_or_default();
    let status = match status_str.as_str() {
        "N" => OrderStatus::New,
        "PF" => OrderStatus::PartiallyFilled,
        "F" => OrderStatus::Filled,
        "C" => OrderStatus::Cancelled,
        other => anyhow::bail!("unknown status from Lua: {other}"),
    };

    let trade_count: usize = redis_value_to_string(&items[3])
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let expected_len = 4 + trade_count * 5;
    if items.len() < expected_len {
        anyhow::bail!(
            "Lua returned {} items but expected {} for {} trades",
            items.len(), expected_len, trade_count
        );
    }

    let mut trades = Vec::with_capacity(trade_count);
    for i in 0..trade_count {
        let base = 4 + i * 5;
        let resting_id_str = redis_value_to_string(&items[base])
            .context("missing resting_id")?;
        let resting_order_id: Uuid = resting_id_str.parse()
            .context("invalid resting_id UUID")?;

        let price_i_str = redis_value_to_string(&items[base + 1])
            .context("missing price_i")?;
        let trade_price_i: i64 = price_i_str.parse().context("invalid price_i")?;

        let qty_i_str = redis_value_to_string(&items[base + 2])
            .context("missing qty_i")?;
        let trade_qty_i: i64 = qty_i_str.parse().context("invalid qty_i")?;

        let buyer_id = redis_value_to_string(&items[base + 3])
            .context("missing buyer_id")?;
        let seller_id = redis_value_to_string(&items[base + 4])
            .context("missing seller_id")?;

        trades.push(LuaTrade {
            resting_order_id,
            price: i64_to_decimal(trade_price_i),
            quantity: i64_to_decimal(trade_qty_i),
            buyer_id,
            seller_id,
        });
    }

    Ok(LuaMatchResult {
        remaining: i64_to_decimal(remaining_i),
        status,
        trades,
    })
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{decimal_to_i64, i64_to_decimal};
    use crate::types::{Order, OrderStatus, OrderType, SelfTradePreventionMode, Side, TimeInForce};
    use chrono::Utc;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use uuid::Uuid;

    fn make_order(side: Side, price: Decimal) -> Order {
        Order {
            id: Uuid::new_v4(),
            user_id: "test-user".to_string(),
            pair_id: "BTC-USDT".to_string(),
            side,
            order_type: OrderType::Limit,
            tif: TimeInForce::GTC,
            price: Some(price),
            quantity: dec!(1),
            remaining: dec!(1),
            status: OrderStatus::New,
            stp_mode: SelfTradePreventionMode::None,
            version: 1,
            sequence: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            client_order_id: None,
            received_at: None,
            matched_at: None,
            persisted_at: None,
        }
    }

    #[test]
    fn test_decimal_to_i64_roundtrip() {
        // Verify decimal_to_i64 and i64_to_decimal are inverses for various values.
        let cases: &[Decimal] = &[
            dec!(0),
            dec!(1.5),
            dec!(0.00000001),
            dec!(99999.99999999),
            dec!(-1.5),
        ];
        for &v in cases {
            let roundtripped = i64_to_decimal(decimal_to_i64(v));
            assert_eq!(roundtripped, v, "roundtrip failed for {v}");
        }
    }

    #[test]
    fn test_decimal_to_i64_precision() {
        // Verify 8 decimal places are preserved.
        let v = dec!(0.12345678);
        let i = decimal_to_i64(v);
        assert_eq!(i, 12_345_678_i64, "0.12345678 * 10^8 should be 12345678");
        let back = i64_to_decimal(i);
        assert_eq!(back, v, "inverse should recover 0.12345678");

        // Minimum representable value (1 satoshi)
        let tiny = dec!(0.00000001);
        assert_eq!(decimal_to_i64(tiny), 1_i64);
        assert_eq!(i64_to_decimal(1_i64), tiny);
    }

    #[test]
    fn test_book_score_consistency() {
        // Bids: higher price → more negative score → ZRANGEBYSCORE returns highest bid first
        let bid_high = make_order(Side::Buy, dec!(200));
        let bid_low  = make_order(Side::Buy, dec!(100));
        assert!(
            bid_high.book_score() < bid_low.book_score(),
            "higher bid price must yield a lower (more negative) score; high={} low={}",
            bid_high.book_score(), bid_low.book_score()
        );

        // Asks: lower price → smaller positive score → ZRANGEBYSCORE returns lowest ask first
        let ask_low  = make_order(Side::Sell, dec!(100));
        let ask_high = make_order(Side::Sell, dec!(200));
        assert!(
            ask_low.book_score() < ask_high.book_score(),
            "lower ask price must yield a smaller score; low={} high={}",
            ask_low.book_score(), ask_high.book_score()
        );

        // Same-price bids have equal scores (Redis stable sort preserves insertion/time order)
        let bid_a = make_order(Side::Buy, dec!(150));
        let bid_b = make_order(Side::Buy, dec!(150));
        assert_eq!(
            bid_a.book_score(), bid_b.book_score(),
            "same-price bids must have equal scores; time priority is handled by insertion order"
        );

        // Buy scores are negative, sell scores are positive
        let bid = make_order(Side::Buy, dec!(100));
        let ask = make_order(Side::Sell, dec!(100));
        assert!(bid.book_score() < 0.0, "bid score must be negative");
        assert!(ask.book_score() > 0.0, "ask score must be positive");
    }
}
