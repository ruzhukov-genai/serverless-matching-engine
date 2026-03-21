//! Worker Lambda — processes a single order from a Lambda event payload.
//!
//! Invoked asynchronously (InvocationType::Event) by the Gateway Lambda.
//! Payload: the same order JSON that the gateway currently pushes to queue:orders:{pair_id}.
//!
//! Flow:
//!   1. Parse order from event payload
//!   2. Validate using pairs_cache (loaded once, cached across warm starts)
//!   3. Lock balance + match via Lua EVAL (atomic, ADR-004)
//!   4. Persist to PG synchronously (simpler than mpsc in Lambda)
//!   5. Update cache keys (orderbook, trades, ticker, portfolio)
//!   6. Return Ok(()) — Lambda framework handles success/failure reporting

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use lambda_runtime::{Error as LambdaError, LambdaEvent, run, service_fn};
use once_cell::sync::OnceCell;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use sme_shared::{
    Order, OrderStatus, OrderType, Side, SelfTradePreventionMode, TimeInForce, Trade,
    cache::{self, PairKeys},
    parse_pair_id,
};

// ── Singleton AppState (warm-start reuse across Lambda invocations) ───────────

/// Minimal AppState for the worker Lambda.
/// Initialized once on first invocation, reused on warm starts.
struct WorkerState {
    dragonfly: deadpool_redis::Pool,
    pg: sqlx::PgPool,
    pairs_cache: Arc<HashMap<String, PairConfig>>,
    pair_keys: Arc<HashMap<String, PairKeys>>,
}

/// Cached pair configuration — loaded once at cold start, never re-queried.
#[derive(Clone, Debug)]
struct PairConfig {
    tick_size: Decimal,
    lot_size: Decimal,
    min_order_size: Decimal,
    max_order_size: Decimal,
    active: bool,
}

static STATE: OnceCell<WorkerState> = OnceCell::new();

async fn get_state() -> Result<&'static WorkerState> {
    if let Some(s) = STATE.get() {
        return Ok(s);
    }

    let dragonfly_url = std::env::var("DRAGONFLY_URL")
        .unwrap_or_else(|_| "redis://localhost:6379".to_string());
    let database_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL env var required")?;

    // Smaller pool sizes for Lambda — connections are per-instance, not global
    let dragonfly = cache::create_pool_sized(&dragonfly_url, 10).await
        .context("failed to create Dragonfly pool")?;

    let pg = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(60))
        .max_lifetime(Duration::from_secs(300))
        .connect(&database_url)
        .await
        .context("failed to connect to PostgreSQL")?;

    let pairs_cache = Arc::new(load_pairs_cache(&pg).await?);
    tracing::info!(count = pairs_cache.len(), "pairs cache loaded");

    let pair_keys: HashMap<String, PairKeys> = pairs_cache.keys()
        .map(|p| (p.clone(), PairKeys::new(p)))
        .collect();
    let pair_keys = Arc::new(pair_keys);

    // Initialize balance cache from PG (needed for Lua balance-lock to work)
    cache::init_balances_from_pg(&dragonfly, &pg).await
        .context("failed to init balance cache")?;

    let state = WorkerState { dragonfly, pg, pairs_cache, pair_keys };

    // Ignore error if another invocation raced us (OnceCell guarantees only one wins)
    let _ = STATE.set(state);
    Ok(STATE.get().expect("just set"))
}

async fn load_pairs_cache(pg: &sqlx::PgPool) -> Result<HashMap<String, PairConfig>> {
    let rows = sqlx::query(
        "SELECT id, tick_size, lot_size, min_order_size, max_order_size, active FROM pairs",
    )
    .fetch_all(pg)
    .await?;

    let mut map = HashMap::with_capacity(rows.len());
    for row in rows {
        let id: String = row.get("id");
        map.insert(id, PairConfig {
            tick_size: row.get("tick_size"),
            lot_size: row.get("lot_size"),
            min_order_size: row.get("min_order_size"),
            max_order_size: row.get("max_order_size"),
            active: row.get("active"),
        });
    }
    Ok(map)
}

// ── Order message format (same as gateway currently pushes to queue) ──────────

#[derive(Deserialize, Debug)]
struct QueuedOrderMsg {
    id: String,
    #[serde(default = "default_user_id")]
    user_id: String,
    pair_id: String,
    side: String,
    order_type: String,
    #[serde(default = "default_gtc")]
    tif: String,
    price: Option<String>,
    quantity: String,
    #[serde(default)]
    stp_mode: Option<String>,
    #[serde(default)]
    client_order_id: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

fn default_gtc() -> String { "GTC".to_string() }
fn default_user_id() -> String { "user-1".to_string() }

// ── Lambda handler ────────────────────────────────────────────────────────────

async fn handler(event: LambdaEvent<Value>) -> Result<Value, LambdaError> {
    let state = get_state().await?;
    let payload = event.payload;

    if let Err(e) = process_order(state, &payload).await {
        tracing::error!(error = %e, "order processing failed");
        // Return error — Lambda will mark this invocation as failed.
        // For async invocations (InvocationType::Event), Lambda may retry.
        return Err(LambdaError::from(e.to_string()));
    }

    Ok(json!({"status": "ok"}))
}

/// Core order processing logic — extracted from sme-api worker.rs process_queued_order().
async fn process_order(state: &WorkerState, payload: &Value) -> Result<()> {
    let total_start = std::time::Instant::now();

    // Parse the order JSON from the Lambda event payload
    let msg: QueuedOrderMsg = serde_json::from_value(payload.clone())
        .context("failed to parse order payload")?;

    let order_id = Uuid::parse_str(&msg.id)
        .context("invalid order id")?;
    let user_id = msg.user_id;
    let pair_id = msg.pair_id;

    let side: Side = match msg.side.as_str() {
        "Buy"  => Side::Buy,
        "Sell" => Side::Sell,
        s => anyhow::bail!("invalid side: {s}"),
    };
    let order_type: OrderType = match msg.order_type.as_str() {
        "Limit"  => OrderType::Limit,
        "Market" => OrderType::Market,
        s => anyhow::bail!("invalid order_type: {s}"),
    };
    let tif: TimeInForce = match msg.tif.as_str() {
        "GTC" => TimeInForce::GTC,
        "IOC" => TimeInForce::IOC,
        "FOK" => TimeInForce::FOK,
        _     => TimeInForce::GTC,
    };
    let stp_mode: SelfTradePreventionMode = match msg.stp_mode.as_deref().unwrap_or("None") {
        "CancelMaker" => SelfTradePreventionMode::CancelMaker,
        "CancelTaker" => SelfTradePreventionMode::CancelTaker,
        "CancelBoth"  => SelfTradePreventionMode::CancelBoth,
        _             => SelfTradePreventionMode::None,
    };

    let price = msg.price.as_deref().and_then(|s| Decimal::from_str(s).ok());
    let quantity = Decimal::from_str(&msg.quantity)
        .map_err(|_| anyhow::anyhow!("invalid quantity: {}", msg.quantity))?;

    let created_at = msg.created_at.as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);

    let now = Utc::now();
    let mut order = Order {
        id: order_id,
        user_id: user_id.clone(),
        pair_id: pair_id.clone(),
        side,
        order_type,
        tif,
        price,
        quantity,
        remaining: quantity,
        status: OrderStatus::New,
        stp_mode,
        version: 1,
        sequence: 0,
        created_at,
        updated_at: now,
        client_order_id: msg.client_order_id,
    };

    // 1. Validate against in-memory pairs cache (zero DB round-trips)
    validate_order(&state.pairs_cache, &order)?;

    // 2. FOK: cancel immediately (not implemented — same as worker.rs)
    if tif == TimeInForce::FOK {
        tracing::warn!("FOK orders not implemented in worker-lambda, cancelling");
        order.status = OrderStatus::Cancelled;
        update_cache_after_processing(state, &order, &[]).await?;
        return Ok(());
    }

    // 3. Compute balance lock parameters
    let (lock_asset, lock_amount_scaled) = {
        let (base, quote) = parse_pair_id(&pair_id)?;
        match order.side {
            Side::Buy => {
                let p = order.price.unwrap_or(Decimal::ZERO);
                (quote, cache::decimal_to_i64(p * order.quantity))
            }
            Side::Sell => (base, cache::decimal_to_i64(order.quantity)),
        }
    };

    // 4. Look up pre-computed pair keys
    let pair_keys = state.pair_keys.get(&pair_id)
        .ok_or_else(|| anyhow::anyhow!("unknown pair_id: {}", pair_id))?;

    // 5. Atomic Lua EVAL: balance lock + matching (ADR-004)
    let match_start = std::time::Instant::now();
    let lua_result = cache::match_order_lua(
        &state.dragonfly,
        &order,
        pair_keys,
        &lock_asset,
        lock_amount_scaled,
    ).await.context("Lua EVAL failed")?;
    let lua_ms = match_start.elapsed().as_millis() as u64;

    order.remaining = lua_result.remaining;
    order.status = lua_result.status;

    // 6. Build Trade objects (UUIDs generated in Rust, not Lua)
    let trade_now = Utc::now();
    let trades: Vec<Trade> = lua_result.trades.iter().map(|lt| {
        let (buy_order_id, sell_order_id) = match order.side {
            Side::Buy  => (order.id, lt.resting_order_id),
            Side::Sell => (lt.resting_order_id, order.id),
        };
        Trade {
            id: Uuid::new_v4(),
            pair_id: order.pair_id.clone(),
            buy_order_id,
            sell_order_id,
            buyer_id: lt.buyer_id.clone(),
            seller_id: lt.seller_id.clone(),
            price: lt.price,
            quantity: lt.quantity,
            sequence: 0,
            created_at: trade_now,
        }
    }).collect();

    let trade_count = trades.len();

    // 7. Persist to PG synchronously (ADR-003: off hot-path allowed, but Lambda
    //    has no background worker, so we do it inline — still fast enough)
    let persist_start = std::time::Instant::now();
    persist_order(state, &order, &trades, &lua_result.trades).await
        .context("DB persist failed")?;
    let persist_ms = persist_start.elapsed().as_millis() as u64;

    // 8. Update cache keys (orderbook, trades, ticker, portfolio)
    update_cache_after_processing(state, &order, &trades).await
        .context("cache update failed")?;

    let total_ms = total_start.elapsed().as_millis() as u64;

    tracing::info!(
        order_id = %order.id,
        pair_id = %order.pair_id,
        side = %msg.side,
        order_type = %msg.order_type,
        status = ?order.status,
        trade_count = trade_count,
        lua_ms = lua_ms,
        persist_ms = persist_ms,
        total_ms = total_ms,
        "order processed"
    );

    Ok(())
}

// ── Validation ────────────────────────────────────────────────────────────────

fn validate_order(pairs_cache: &HashMap<String, PairConfig>, order: &Order) -> Result<()> {
    let cfg = pairs_cache.get(&order.pair_id)
        .ok_or_else(|| anyhow::anyhow!("unknown pair {}", order.pair_id))?;

    if !cfg.active {
        anyhow::bail!("pair {} is not active", order.pair_id);
    }
    if order.quantity % cfg.lot_size != Decimal::ZERO {
        anyhow::bail!("quantity not aligned to lot_size");
    }
    if order.quantity < cfg.min_order_size || order.quantity > cfg.max_order_size {
        anyhow::bail!("quantity out of range [{}, {}]", cfg.min_order_size, cfg.max_order_size);
    }
    if let Some(price) = order.price {
        if cfg.tick_size > Decimal::ZERO && price % cfg.tick_size != Decimal::ZERO {
            anyhow::bail!("price not aligned to tick_size");
        }
    }
    Ok(())
}

// ── DB persistence ────────────────────────────────────────────────────────────

/// Persist the matched order and its trades to PostgreSQL.
/// In Lambda we do this synchronously (no mpsc background worker available).
/// Uses a single transaction for atomicity.
async fn persist_order(
    state: &WorkerState,
    order: &Order,
    trades: &[Trade],
    lua_trades: &[cache::LuaTrade],
) -> Result<()> {
    let mut tx = state.pg.begin().await?;

    // Insert the incoming order
    sqlx::query(
        "INSERT INTO orders (id, user_id, pair_id, side, order_type, tif, price, quantity, remaining, status, stp_mode, version, created_at, updated_at, client_order_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
         ON CONFLICT DO NOTHING",
    )
    .bind(order.id)
    .bind(&order.user_id)
    .bind(&order.pair_id)
    .bind(side_str(order.side))
    .bind(order_type_str(order.order_type))
    .bind(tif_str(order.tif))
    .bind(order.price)
    .bind(order.quantity)
    .bind(order.remaining)
    .bind(status_str(order.status))
    .bind(stp_str(order.stp_mode))
    .bind(order.version)
    .bind(order.created_at)
    .bind(order.updated_at)
    .bind(&order.client_order_id)
    .execute(&mut *tx)
    .await?;

    // Insert all trades and aggregate balance deltas
    let mut balance_deltas: HashMap<(String, String), (Decimal, Decimal)> = HashMap::new();

    for trade in trades {
        sqlx::query(
            "INSERT INTO trades (id, pair_id, buy_order_id, sell_order_id, buyer_id, seller_id, price, quantity, created_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT DO NOTHING",
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
        .execute(&mut *tx)
        .await?;

        let (base, quote) = parse_pair_id(&trade.pair_id)?;
        let cost = trade.price * trade.quantity;

        // buyer: locked -= cost, available += qty (base)
        balance_deltas.entry((trade.buyer_id.clone(), quote.clone()))
            .or_insert((Decimal::ZERO, Decimal::ZERO)).0 += cost;
        balance_deltas.entry((trade.buyer_id.clone(), base.clone()))
            .or_insert((Decimal::ZERO, Decimal::ZERO)).1 += trade.quantity;

        // seller: locked -= qty (base), available += cost
        balance_deltas.entry((trade.seller_id.clone(), base.clone()))
            .or_insert((Decimal::ZERO, Decimal::ZERO)).0 += trade.quantity;
        balance_deltas.entry((trade.seller_id.clone(), quote.clone()))
            .or_insert((Decimal::ZERO, Decimal::ZERO)).1 += cost;
    }

    // Apply balance deltas (sorted for consistent lock ordering = no deadlock)
    let mut sorted_deltas: Vec<_> = balance_deltas.iter().collect();
    sorted_deltas.sort_by_key(|((uid, asset), _)| (uid.as_str(), asset.as_str()));

    for ((user_id, asset), (locked_decrease, available_increase)) in &sorted_deltas {
        sqlx::query(
            "INSERT INTO balances (user_id, asset, available, locked) VALUES ($3, $4, $2, 0)
             ON CONFLICT (user_id, asset) DO UPDATE
               SET locked    = GREATEST(balances.locked    - $1, 0),
                   available = balances.available + $2",
        )
        .bind(locked_decrease)
        .bind(available_increase)
        .bind(user_id.as_str())
        .bind(asset.as_str())
        .execute(&mut *tx)
        .await?;
    }

    // Update resting orders after Lua fill
    let now = Utc::now();
    for lt in lua_trades {
        sqlx::query(
            "UPDATE orders
             SET remaining   = GREATEST(remaining - $1, 0),
                 status      = CASE WHEN GREATEST(remaining - $1, 0) = 0
                                    THEN 'Filled' ELSE 'PartiallyFilled' END,
                 version     = version + 1,
                 updated_at  = $2
             WHERE id = $3
               AND status IN ('New', 'PartiallyFilled')",
        )
        .bind(lt.quantity)
        .bind(now)
        .bind(lt.resting_order_id)
        .execute(&mut *tx)
        .await?;
    }

    // Update incoming order's remaining + status
    sqlx::query(
        "UPDATE orders SET remaining = $1, status = $2, version = version + 1, updated_at = $3 WHERE id = $4",
    )
    .bind(order.remaining)
    .bind(status_str(order.status))
    .bind(now)
    .bind(order.id)
    .execute(&mut *tx)
    .await?;

    // Release remaining locked balance if order fully resolved
    if order.status == OrderStatus::Cancelled || order.status == OrderStatus::Filled {
        if order.remaining != Decimal::ZERO {
            let (base, quote) = parse_pair_id(&order.pair_id)?;
            let (asset, amount) = match order.side {
                Side::Buy  => {
                    let p = order.price.unwrap_or(Decimal::ZERO);
                    (quote, p * order.remaining)
                }
                Side::Sell => (base, order.remaining),
            };
            sqlx::query(
                "UPDATE balances SET available = available + $1, locked = GREATEST(locked - $1, 0) WHERE user_id = $2 AND asset = $3",
            )
            .bind(amount)
            .bind(&order.user_id)
            .bind(&asset)
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;

    // Audit events — fire-and-forget outside main transaction
    let _ = sqlx::query("INSERT INTO audit_log (pair_id, event_type, payload) VALUES ($1, $2, $3)")
        .bind(&order.pair_id)
        .bind("ORDER_CREATED")
        .bind(json!({
            "order_id": order.id.to_string(),
            "user_id": order.user_id,
            "side": side_str(order.side),
            "order_type": order_type_str(order.order_type),
            "price": order.price.map(|v| v.to_string()),
            "quantity": order.quantity.to_string(),
        }))
        .execute(&state.pg)
        .await;

    for trade in trades {
        let _ = sqlx::query("INSERT INTO audit_log (pair_id, event_type, payload) VALUES ($1, $2, $3)")
            .bind(&trade.pair_id)
            .bind("TRADE_EXECUTED")
            .bind(json!({
                "trade_id": trade.id.to_string(),
                "buy_order_id": trade.buy_order_id.to_string(),
                "sell_order_id": trade.sell_order_id.to_string(),
                "price": trade.price.to_string(),
                "quantity": trade.quantity.to_string(),
            }))
            .execute(&state.pg)
            .await;
    }

    Ok(())
}

// ── Cache updates ─────────────────────────────────────────────────────────────

/// Update Dragonfly cache keys after an order is processed.
/// Rebuilds orderbook snapshot using Lua (single round-trip).
async fn update_cache_after_processing(
    state: &WorkerState,
    order: &Order,
    trades: &[Trade],
) -> Result<()> {
    let had_trades = !trades.is_empty();
    let order_rested = order.status == OrderStatus::New
        || order.status == OrderStatus::PartiallyFilled;

    if !had_trades && !order_rested {
        // No book change — skip reload
        return Ok(());
    }

    // Rebuild orderbook snapshot (Lua single round-trip)
    let (bids, asks) = cache::orderbook_snapshot_lua(&state.dragonfly, &order.pair_id, 50).await?;

    let bids_json: Vec<Value> = bids.iter().map(|(p, q)| json!([p, q])).collect();
    let asks_json: Vec<Value> = asks.iter().map(|(p, q)| json!([p, q])).collect();

    let orderbook_json = serde_json::to_string(&json!({
        "pair": order.pair_id,
        "bids": bids_json,
        "asks": asks_json,
    }))?;
    cache::set_and_publish(&state.dragonfly, &format!("cache:orderbook:{}", order.pair_id), &orderbook_json).await?;

    // Update trades cache from PG (last 50)
    update_trades_cache(state, &order.pair_id).await?;

    // Update ticker cache from PG
    update_ticker_cache(state, &order.pair_id).await?;

    // Update portfolio caches for all affected users
    let mut dirty_users = std::collections::HashSet::new();
    dirty_users.insert(order.user_id.clone());
    for trade in trades {
        dirty_users.insert(trade.buyer_id.clone());
        dirty_users.insert(trade.seller_id.clone());
    }
    update_portfolio_caches(state, &dirty_users).await?;

    Ok(())
}

async fn update_trades_cache(state: &WorkerState, pair_id: &str) -> Result<()> {
    let rows = sqlx::query(
        "SELECT id, pair_id, buy_order_id, sell_order_id, buyer_id, seller_id, price, quantity, sequence, created_at
         FROM trades WHERE pair_id = $1 ORDER BY created_at DESC LIMIT 50",
    )
    .bind(pair_id)
    .fetch_all(&state.pg)
    .await?;

    let trades: Vec<Value> = rows.iter().map(|r| {
        json!({
            "id": r.get::<Uuid, _>("id").to_string(),
            "pair_id": r.get::<String, _>("pair_id"),
            "buy_order_id": r.get::<Uuid, _>("buy_order_id").to_string(),
            "sell_order_id": r.get::<Uuid, _>("sell_order_id").to_string(),
            "buyer_id": r.get::<String, _>("buyer_id"),
            "seller_id": r.get::<String, _>("seller_id"),
            "price": r.get::<Decimal, _>("price").to_string(),
            "quantity": r.get::<Decimal, _>("quantity").to_string(),
            "created_at": r.get::<chrono::DateTime<Utc>, _>("created_at").to_rfc3339(),
        })
    }).collect();

    let trades_str = serde_json::to_string(&json!({ "pair": pair_id, "trades": trades }))?;
    cache::set_and_publish(&state.dragonfly, &format!("cache:trades:{}", pair_id), &trades_str).await?;
    Ok(())
}

async fn update_ticker_cache(state: &WorkerState, pair_id: &str) -> Result<()> {
    let ticker_row = sqlx::query(
        "SELECT
            MAX(price) FILTER (WHERE created_at >= NOW() - INTERVAL '24 hours') as high_24h,
            MIN(price) FILTER (WHERE created_at >= NOW() - INTERVAL '24 hours') as low_24h,
            SUM(quantity) FILTER (WHERE created_at >= NOW() - INTERVAL '24 hours') as volume_24h
         FROM trades WHERE pair_id = $1",
    )
    .bind(pair_id)
    .fetch_optional(&state.pg)
    .await?;

    let last_row = sqlx::query(
        "SELECT price as last_price FROM trades WHERE pair_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(pair_id)
    .fetch_optional(&state.pg)
    .await?;

    let last = last_row.and_then(|r| r.get::<Option<Decimal>, _>("last_price")).map(|v| v.to_string());

    let ticker_json = if let Some(row) = ticker_row {
        json!({
            "pair": pair_id,
            "last": last,
            "high_24h": row.get::<Option<Decimal>, _>("high_24h").map(|v| v.to_string()),
            "low_24h": row.get::<Option<Decimal>, _>("low_24h").map(|v| v.to_string()),
            "volume_24h": row.get::<Option<Decimal>, _>("volume_24h").map(|v| v.to_string()),
        })
    } else {
        json!({ "pair": pair_id, "last": last, "high_24h": null, "low_24h": null, "volume_24h": null })
    };

    let ticker_str = serde_json::to_string(&ticker_json)?;
    cache::set_and_publish(&state.dragonfly, &format!("cache:ticker:{}", pair_id), &ticker_str).await?;
    Ok(())
}

async fn update_portfolio_caches(
    state: &WorkerState,
    user_ids: &std::collections::HashSet<String>,
) -> Result<()> {
    let mut conn = state.dragonfly.get().await?;
    for user_id in user_ids {
        let rows = sqlx::query(
            "SELECT user_id, asset, available, locked FROM balances WHERE user_id = $1 ORDER BY asset",
        )
        .bind(user_id)
        .fetch_all(&state.pg)
        .await?;

        let balances: Vec<Value> = rows.iter().map(|r| {
            json!({
                "user_id": r.get::<String, _>("user_id"),
                "asset": r.get::<String, _>("asset"),
                "available": r.get::<Decimal, _>("available").to_string(),
                "locked": r.get::<Decimal, _>("locked").to_string(),
            })
        }).collect();

        let portfolio_str = serde_json::to_string(&json!({"balances": balances}))?;
        use deadpool_redis::redis::AsyncCommands;
        conn.set::<_, _, ()>(format!("cache:portfolio:{}", user_id), &portfolio_str).await?;
    }
    Ok(())
}

// ── Static string helpers ─────────────────────────────────────────────────────

fn side_str(s: Side) -> &'static str {
    match s { Side::Buy => "Buy", Side::Sell => "Sell" }
}
fn order_type_str(ot: OrderType) -> &'static str {
    match ot { OrderType::Limit => "Limit", OrderType::Market => "Market" }
}
fn tif_str(tif: TimeInForce) -> &'static str {
    match tif { TimeInForce::GTC => "GTC", TimeInForce::IOC => "IOC", TimeInForce::FOK => "FOK" }
}
fn status_str(s: OrderStatus) -> &'static str {
    match s {
        OrderStatus::New             => "New",
        OrderStatus::PartiallyFilled => "PartiallyFilled",
        OrderStatus::Filled          => "Filled",
        OrderStatus::Cancelled       => "Cancelled",
        OrderStatus::Rejected        => "Rejected",
    }
}
fn stp_str(s: SelfTradePreventionMode) -> &'static str {
    match s {
        SelfTradePreventionMode::None         => "None",
        SelfTradePreventionMode::CancelMaker  => "CancelMaker",
        SelfTradePreventionMode::CancelTaker  => "CancelTaker",
        SelfTradePreventionMode::CancelBoth   => "CancelBoth",
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), LambdaError> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .without_time()   // Lambda adds its own timestamp
        .init();

    tracing::info!("sme-worker-lambda starting");

    run(service_fn(handler)).await
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_queued_order_msg_full() {
        let payload = json!({
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "user_id": "user-1",
            "pair_id": "BTC-USDT",
            "side": "Buy",
            "order_type": "Limit",
            "tif": "GTC",
            "price": "50000.00",
            "quantity": "0.001",
            "stp_mode": "None",
            "client_order_id": "my-order-1",
            "created_at": "2026-03-21T00:00:00Z",
        });
        let msg: QueuedOrderMsg = serde_json::from_value(payload).unwrap();
        assert_eq!(msg.pair_id, "BTC-USDT");
        assert_eq!(msg.side, "Buy");
        assert_eq!(msg.order_type, "Limit");
        assert_eq!(msg.tif, "GTC");
        assert_eq!(msg.price.as_deref(), Some("50000.00"));
        assert_eq!(msg.quantity, "0.001");
        assert_eq!(msg.user_id, "user-1");
        assert_eq!(msg.client_order_id.as_deref(), Some("my-order-1"));
    }

    #[test]
    fn test_parse_queued_order_msg_minimal() {
        // Minimal payload — only required fields
        let payload = json!({
            "id": "550e8400-e29b-41d4-a716-446655440001",
            "pair_id": "ETH-USDT",
            "side": "Sell",
            "order_type": "Market",
            "quantity": "0.1",
        });
        let msg: QueuedOrderMsg = serde_json::from_value(payload).unwrap();
        assert_eq!(msg.pair_id, "ETH-USDT");
        assert_eq!(msg.side, "Sell");
        assert_eq!(msg.tif, "GTC"); // default
        assert_eq!(msg.user_id, "user-1"); // default
        assert!(msg.price.is_none());
        assert!(msg.stp_mode.is_none());
        assert!(msg.client_order_id.is_none());
    }

    #[test]
    fn test_parse_order_types() {
        // Side parsing
        let buy_side: Side = match "Buy" {
            "Buy" => Side::Buy,
            "Sell" => Side::Sell,
            _ => panic!("bad side"),
        };
        assert!(matches!(buy_side, Side::Buy));

        let sell_side: Side = match "Sell" {
            "Buy" => Side::Buy,
            "Sell" => Side::Sell,
            _ => panic!("bad side"),
        };
        assert!(matches!(sell_side, Side::Sell));
    }

    #[test]
    fn test_validate_order_unknown_pair() {
        let pairs_cache: HashMap<String, PairConfig> = HashMap::new();
        let order = Order {
            id: Uuid::new_v4(),
            user_id: "user-1".to_string(),
            pair_id: "UNKNOWN-PAIR".to_string(),
            side: Side::Buy,
            order_type: OrderType::Limit,
            tif: TimeInForce::GTC,
            price: Some(Decimal::from(100)),
            quantity: Decimal::from(1),
            remaining: Decimal::from(1),
            status: OrderStatus::New,
            stp_mode: SelfTradePreventionMode::None,
            version: 1,
            sequence: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            client_order_id: None,
        };
        let result = validate_order(&pairs_cache, &order);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown pair"));
    }

    #[test]
    fn test_validate_order_inactive_pair() {
        let mut pairs_cache: HashMap<String, PairConfig> = HashMap::new();
        pairs_cache.insert("BTC-USDT".to_string(), PairConfig {
            tick_size: Decimal::from_str("0.01").unwrap(),
            lot_size: Decimal::from_str("0.00001").unwrap(),
            min_order_size: Decimal::from_str("0.00001").unwrap(),
            max_order_size: Decimal::from_str("100").unwrap(),
            active: false,
        });
        let order = Order {
            id: Uuid::new_v4(),
            user_id: "user-1".to_string(),
            pair_id: "BTC-USDT".to_string(),
            side: Side::Buy,
            order_type: OrderType::Limit,
            tif: TimeInForce::GTC,
            price: Some(Decimal::from_str("50000.00").unwrap()),
            quantity: Decimal::from_str("0.001").unwrap(),
            remaining: Decimal::from_str("0.001").unwrap(),
            status: OrderStatus::New,
            stp_mode: SelfTradePreventionMode::None,
            version: 1,
            sequence: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            client_order_id: None,
        };
        let result = validate_order(&pairs_cache, &order);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not active"));
    }

    #[test]
    fn test_static_string_helpers() {
        assert_eq!(side_str(Side::Buy), "Buy");
        assert_eq!(side_str(Side::Sell), "Sell");
        assert_eq!(order_type_str(OrderType::Limit), "Limit");
        assert_eq!(order_type_str(OrderType::Market), "Market");
        assert_eq!(tif_str(TimeInForce::GTC), "GTC");
        assert_eq!(tif_str(TimeInForce::IOC), "IOC");
        assert_eq!(tif_str(TimeInForce::FOK), "FOK");
        assert_eq!(status_str(OrderStatus::New), "New");
        assert_eq!(status_str(OrderStatus::Filled), "Filled");
        assert_eq!(status_str(OrderStatus::Cancelled), "Cancelled");
        assert_eq!(stp_str(SelfTradePreventionMode::None), "None");
        assert_eq!(stp_str(SelfTradePreventionMode::CancelMaker), "CancelMaker");
    }
}
