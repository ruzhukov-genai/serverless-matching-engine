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

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use deadpool_redis::{Config as DPConfig, Pool, Runtime};
use deadpool_redis::redis::AsyncCommands;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
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

/// Create a Dragonfly connection pool with a specific max size.
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

/// SET a cache key in Dragonfly AND PUBLISH the update for gateway subscribers.
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
    let score = order.book_score();
    let book_key = match order.side { Side::Buy => &bids_key, Side::Sell => &asks_key };

    let price_i = order.price.map(decimal_to_i64).unwrap_or(0);

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
            let f: f64 = price.to_string().parse().unwrap_or(0.0);
            match side {
                Side::Sell => ("-inf".to_string(), f.to_string()),
                Side::Buy => ("-inf".to_string(), (-f).to_string()),
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
            let f: f64 = price.to_string().parse().unwrap_or(0.0);
            match side {
                Side::Sell => ("-inf".to_string(), f.to_string()),
                Side::Buy => ("-inf".to_string(), (-f).to_string()),
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
/// All accessed keys are declared in KEYS[] (Dragonfly/Redis cluster compatibility).
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
        mutations.push(Mutation {
            op,
            oid: upd.id.to_string(),
            order_key: format!("order:{}", upd.id),
            score: upd.book_score(),
        });
    }

    if let Some(incoming) = incoming_if_resting {
        let op = match incoming.side {
            Side::Buy => "UPSERT_BID",
            Side::Sell => "UPSERT_ASK",
        };
        upsert_orders.push(incoming);
        mutations.push(Mutation {
            op,
            oid: incoming.id.to_string(),
            order_key: format!("order:{}", incoming.id),
            score: incoming.book_score(),
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
    let script = redis::Script::new(CAS_SCRIPT);
    let mut invocation = script.prepare_invoke();
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

/// Match an incoming order atomically inside Dragonfly via a single EVAL round-trip.
///
/// The Lua script calls ZRANGEBYSCORE internally to fetch resting order IDs,
/// then accesses their hashes directly. Dragonfly (non-cluster) allows undeclared
/// key access in scripts — no Phase 1 round-trip needed.
///
/// Total: 1 round-trip, no lock, no retry, no CAS version check.
///
/// # FOK note
/// FOK orders are NOT handled here. See the OCC/CAS path in routes.rs.
pub async fn match_order_lua(
    pool: &Pool,
    order: &Order,
) -> Result<LuaMatchResult> {
    let lua_script = include_str!("lua/match_order.lua");

    let bids_key = format!("book:{}:bids", order.pair_id);
    let asks_key = format!("book:{}:asks", order.pair_id);
    let version_key = format!("version:{}", order.pair_id);
    let incoming_order_key = format!("order:{}", order.id);

    let price_i = order.price.map(decimal_to_i64).unwrap_or(0);
    let score = order.book_score();

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

    // Single Lua EVAL — ZRANGEBYSCORE + matching all in one atomic call
    // KEYS: [1]=bids, [2]=asks, [3]=version, [4]=order:{incoming_id}
    // ARGV: [1..11]=order fields, [12]=max_score
    let script = redis::Script::new(lua_script);
    let mut invocation = script.prepare_invoke();

    invocation
        .key(&bids_key)
        .key(&asks_key)
        .key(&version_key)
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
        .arg(score.to_string())
        .arg(&max_score);

    let raw: redis::Value = invocation
        .invoke_async(&mut *conn)
        .await
        .context("Lua match_order EVAL")?;

    // Parse the returned bulk array:
    //   [0] = "OK"
    //   [1] = remaining_i
    //   [2] = status
    //   [3] = trade_count
    //   then 5 fields per trade: resting_id, price_i, qty_i, buyer_id, seller_id
    let items = match raw {
        redis::Value::Array(v) => v,
        other => anyhow::bail!("unexpected Lua return type: {:?}", other),
    };

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
