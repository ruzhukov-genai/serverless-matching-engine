//! Order Book cache operations — sorted sets for bids/asks.
//!
//! Keys:
//!   book:{pair_id}:bids  → ZSet (score = -price, so ZRANGEBYSCORE -inf +inf = highest bid first)
//!   book:{pair_id}:asks  → ZSet (score = +price, so ZRANGEBYSCORE -inf +inf = lowest ask first)
//!   order:{order_id}     → Hash (field "data" = MessagePack-serialized Order)
//!
//! MessagePack is ~30-40% smaller and ~2x faster to (de)serialize vs JSON.

use std::time::Duration;

use anyhow::{Context, Result};
use deadpool_redis::{Config as DPConfig, Pool, Runtime};
use redis::AsyncCommands;
use uuid::Uuid;

use crate::types::{Order, Side};

// ── Pool ─────────────────────────────────────────────────────────────────────

pub async fn create_pool(url: &str) -> Result<Pool> {
    let pool = DPConfig::from_url(url)
        .builder()
        .context("failed to create redis pool builder")?
        .max_size(50)
        .wait_timeout(Some(Duration::from_secs(3)))
        .runtime(Runtime::Tokio1)
        .build()
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

/// Fetch multiple orders by IDs using pipelined HGET commands to avoid N+1 calls.
async fn fetch_orders_by_ids(pool: &Pool, ids: &[String]) -> Result<Vec<Order>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut conn = pool.get().await.context("pool.get")?;
    let mut pipeline = redis::pipe();
    
    // Build pipeline with all HGET commands
    for id in ids {
        let okey = format!("order:{id}");
        pipeline.hget(&okey, "data");
    }
    
    // Execute pipeline and get all results at once (binary MessagePack data)
    let data_list: Vec<Option<Vec<u8>>> = pipeline
        .query_async(&mut *conn)
        .await
        .context("pipeline HGET")?;
    
    let mut orders = Vec::with_capacity(ids.len());
    for (id, data) in ids.iter().zip(data_list.iter()) {
        if let Some(bytes) = data {
            match rmp_serde::from_slice::<Order>(bytes) {
                Ok(order) => orders.push(order),
                Err(e) => tracing::warn!("bad order msgpack for {id}: {e}"),
            }
        }
    }
    
    Ok(orders)
}

/// Score for the sorted set.  
/// Bids: negate price so ZRANGEBYSCORE -inf +inf returns highest bid first.  
/// Asks: raw price so ZRANGEBYSCORE -inf +inf returns lowest ask first.


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

    fetch_orders_by_ids(pool, &ids).await
}

/// Save an order to the sorted set + hash.
pub async fn save_order_to_book(pool: &Pool, order: &Order) -> Result<()> {
    let mut conn = pool.get().await.context("pool.get")?;
    let score = order.book_score();
    let zkey = book_key(&order.pair_id, order.side);
    let okey = order_key(&order.id);
    let bytes = rmp_serde::to_vec_named(order).context("serialize order to msgpack")?;

    // ZADD + HSET in pipeline
    let () = redis::pipe()
        .cmd("ZADD")
        .arg(&zkey)
        .arg(score)
        .arg(order.id.to_string())
        .cmd("HSET")
        .arg(&okey)
        .arg("data")
        .arg(&bytes)
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
    let data: Option<Vec<u8>> = conn.hget(&okey, "data").await.context("HGET")?;
    Ok(match data {
        Some(bytes) => Some(rmp_serde::from_slice(&bytes).context("deserialize order msgpack")?),
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

/// Load orders from one side with optional price filter.
///
/// For a Buy incoming order (price = limit):
///   - Load asks with score <= buy_price (cheapest asks that qualify).
/// For a Sell incoming order (price = limit):
///   - Load bids with score >= -sell_price (i.e. bids where -score <= sell_price → bid price >= sell_price).
/// If `limit_price` is None (market order), load all orders.
pub async fn load_order_book_filtered(
    pool: &Pool,
    pair_id: &str,
    side: Side,
    limit_price: Option<rust_decimal::Decimal>,
) -> Result<Vec<Order>> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = book_key(pair_id, side);

    let (min_score, max_score) = match limit_price {
        None => ("-inf".to_string(), "+inf".to_string()),
        Some(price) => {
            let f: f64 = price.to_string().parse().unwrap_or(0.0);
            match side {
                // asks: score = +price, we want asks where score <= buy_price
                Side::Sell => ("-inf".to_string(), f.to_string()),
                // bids: score = -price, we want bids where -score >= sell_price → score <= -sell_price
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

/// Load a batch of orders from one side using ZRANGEBYSCORE with LIMIT (for lazy loading).
/// `offset` and `count` implement pagination through the book.
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

// ── OCC / CAS primitives ──────────────────────────────────────────────────────

/// Result of a compare-and-swap attempt.
pub enum CasResult {
    /// Mutations applied atomically, version incremented.
    Ok,
    /// Version changed between snapshot and CAS — caller should retry.
    Conflict,
}

/// Load the order book snapshot AND the current version in a single pipeline.
///
/// Returns `(orders, version)` where `version` is the monotonic counter at
/// `version:{pair_id}`. The version must be passed unchanged to
/// `apply_book_mutations_cas` so the Lua script can verify no concurrent
/// mutation occurred between snapshot and CAS.
pub async fn load_order_book_snapshot(
    pool: &Pool,
    pair_id: &str,
    side: Side,
    limit_price: Option<rust_decimal::Decimal>,
) -> Result<(Vec<Order>, i64)> {
    let mut conn = pool.get().await.context("pool.get")?;
    let zkey = book_key(pair_id, side);
    let version_key = format!("version:{pair_id}");

    let (min_score, max_score) = match limit_price {
        None => ("-inf".to_string(), "+inf".to_string()),
        Some(price) => {
            let f: f64 = price.to_string().parse().unwrap_or(0.0);
            match side {
                // asks: score = +price, we want asks where score <= buy_price
                Side::Sell => ("-inf".to_string(), f.to_string()),
                // bids: score = -price, we want bids where score <= -sell_price
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

    // Pipeline all HGET calls for the returned order IDs
    let mut hget_pipe = redis::pipe();
    for id in &ids {
        let okey = format!("order:{id}");
        hget_pipe.hget(&okey, "data");
    }
    let data_list: Vec<Option<Vec<u8>>> = hget_pipe
        .query_async(&mut *conn)
        .await
        .context("pipeline HGET orders")?;

    let mut orders = Vec::with_capacity(ids.len());
    for (id, data) in ids.iter().zip(data_list.iter()) {
        if let Some(bytes) = data {
            match rmp_serde::from_slice::<Order>(bytes) {
                Ok(order) => orders.push(order),
                Err(e) => tracing::warn!("bad order msgpack for {id}: {e}"),
            }
        }
    }

    Ok((orders, version))
}

/// The Lua CAS script — atomically checks version, applies ZSet mutations, increments version.
///
/// All accessed keys are declared in KEYS[] (Dragonfly/Redis cluster compatibility).
/// Binary msgpack data is NOT passed through Lua — callers pre-write HSET data via pipeline.
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
        redis.call('HDEL', okey, 'data')
    elseif op == 'REMOVE_ASK' then
        redis.call('ZREM', KEYS[3], oid)
        redis.call('HDEL', okey, 'data')
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
/// 1. Pre-write all HSET (order hash) data for UPSERT ops in a pipeline (outside Lua).
/// 2. Lua CAS script: check version, ZADD/ZREM, INCR version.
///    All accessed Redis keys are declared in KEYS[] for Dragonfly compatibility.
///
/// If CAS conflicts, the pre-written HSET data may be orphaned temporarily.
/// It will be cleaned up by a subsequent REMOVE_* or overwritten on next retry.
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
                if upd.status == crate::types::OrderStatus::Filled
                    || upd.status == crate::types::OrderStatus::Cancelled
                {
                    ("REMOVE_BID", false)
                } else {
                    ("UPSERT_BID", true)
                }
            }
            Side::Sell => {
                if upd.status == crate::types::OrderStatus::Filled
                    || upd.status == crate::types::OrderStatus::Cancelled
                {
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

    // Phase 1: Pre-write HSET data for all UPSERT ops (pipeline, no Lua involved)
    if !upsert_orders.is_empty() {
        let mut hset_pipe = redis::pipe();
        for order in &upsert_orders {
            let okey = format!("order:{}", order.id);
            let bytes = rmp_serde::to_vec_named(order).context("serialize order to msgpack")?;
            hset_pipe.cmd("HSET").arg(&okey).arg("data").arg(bytes.as_slice()).ignore();
        }
        hset_pipe.query_async::<()>(&mut *conn).await.context("pre-write HSET pipeline")?;
    }

    // Phase 2: Lua CAS — version check + ZADD/ZREM + INCR
    // All keys (including order hash keys) declared in KEYS[] for Dragonfly compatibility
    let n = mutations.len();
    let script = redis::Script::new(CAS_SCRIPT);
    let mut invocation = script.prepare_invoke();
    invocation
        .key(&version_key)
        .key(&bids_key)
        .key(&asks_key);

    // Add one order hash key per mutation (KEYS[4..4+N-1])
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

