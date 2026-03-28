//! Inline order processing -- worker logic embedded in the Gateway Lambda.
//!
//! Extracted from crates/worker-lambda/src/main.rs so that when
//! ORDER_DISPATCH_MODE=inline the Gateway can run matching + persistence
//! in-process without a cross-Lambda invoke (~800ms overhead).
//!
//! Key constraints (ADRs):
//!   ADR-001: Workers must be stateless -- all mutable state in Valkey/PG.
//!   ADR-004: Matching MUST use Lua EVAL -- do not bypass cache::match_order_lua.
//!   CLAUDE.md: Use sqlx::query() with .bind() -- never sqlx::query! macro.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use once_cell::sync::OnceCell;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Row, QueryBuilder, Postgres};
use uuid::Uuid;

use sme_shared::{
    Order, OrderStatus, OrderType, Side, SelfTradePreventionMode, TimeInForce, Trade,
    cache::{self, PairKeys},
    parse_pair_id,
};

// AWS SDK imports for WebSocket push (optional, non-critical)
use aws_sdk_apigatewaymanagement;

pub struct WorkerState {
    pub redis: deadpool_redis::Pool,
    pub pg: sqlx::PgPool,
    pub pairs_cache: Arc<HashMap<String, PairConfig>>,
    pub pair_keys: Arc<HashMap<String, PairKeys>>,
}

/// Cached pair configuration — loaded once at cold start, never re-queried.
#[derive(Clone, Debug)]
pub struct PairConfig {
    pub tick_size: Decimal,
    pub lot_size: Decimal,
    pub min_order_size: Decimal,
    pub max_order_size: Decimal,
    pub active: bool,
}

static STATE: OnceCell<WorkerState> = OnceCell::new();

pub async fn get_state() -> Result<&'static WorkerState> {
    if let Some(s) = STATE.get() {
        return Ok(s);
    }

    tracing::info!("get_state: initializing worker (cold start)");

    let redis_url = std::env::var("REDIS_URL")
        .unwrap_or_else(|_| "redis://localhost:6379".to_string());
    let database_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL env var required")?;

    // Minimal pool for Lambda — each instance processes one order at a time
    tracing::info!("get_state: connecting to Valkey");
    let redis = cache::create_pool_sized(&redis_url, 2).await
        .context("failed to create Valkey pool")?;

    tracing::info!("get_state: creating PG pool (lazy)");
    let pg = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .min_connections(0)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(60))
        .max_lifetime(Duration::from_secs(300))
        .connect_lazy(&database_url)
        .context("failed to create PostgreSQL pool")?;
    tracing::info!("get_state: PG pool created (lazy, no connection yet)");

    // Note: DB migrations are run explicitly via deploy scripts or migrate tool
    // to avoid connection stampede during cold starts

    let pairs_cache = Arc::new(load_pairs_cache(&pg).await?);
    tracing::info!(count = pairs_cache.len(), "pairs cache loaded");

    let pair_keys: HashMap<String, PairKeys> = pairs_cache.keys()
        .map(|p| (p.clone(), PairKeys::new(p)))
        .collect();
    let pair_keys = Arc::new(pair_keys);

    // Seed caches from PG only if not already populated.
    // Uses a sentinel key to avoid 40+ concurrent Lambdas all trying to seed simultaneously.
    // The first Lambda to cold-start seeds; subsequent ones skip.
    let needs_seed = {
        let mut conn = redis.get().await.context("pool.get for seed check")?;
        let exists: bool = redis::cmd("EXISTS")
            .arg("cache:pairs")
            .query_async(&mut *conn)
            .await
            .unwrap_or(false);
        !exists
    };

    if needs_seed {
        tracing::info!("cache not populated — seeding from PG");

        // Initialize balance cache from PG (needed for Lua balance-lock to work)
        cache::init_balances_from_pg(&redis, &pg).await
            .context("failed to init balance cache")?;

        // Seed order book ZSETs from PG resting orders
        cache::seed_orderbook_from_pg(&redis, &pg).await
            .context("failed to seed orderbook from PG")?;

        // Seed cache:pairs (Gateway reads this for /api/pairs)
        seed_pairs_cache(&redis, &pg).await
            .context("failed to seed cache:pairs")?;

        // Seed ticker cache from recent trades
        let pair_ids: Vec<String> = pairs_cache.keys().cloned().collect();
        cache::seed_ticker_from_pg(&redis, &pg, &pair_ids).await
            .context("failed to seed ticker cache from PG")?;

        tracing::info!("cache seeding complete");
    } else {
        tracing::info!("cache already populated — skipping seed");
    }

    let state = WorkerState { redis, pg, pairs_cache, pair_keys };

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

/// Seed cache:pairs from PG so the Gateway's /api/pairs endpoint returns valid JSON.
async fn seed_pairs_cache(redis: &deadpool_redis::Pool, pg: &sqlx::PgPool) -> Result<()> {
    let rows = sqlx::query(
        "SELECT id, base, quote, tick_size, lot_size, min_order_size, max_order_size, \
         price_precision, qty_precision, price_band_pct, active \
         FROM pairs WHERE active = true",
    )
    .fetch_all(pg)
    .await?;

    let pairs: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<String, _>("id"),
                "base": r.get::<String, _>("base"),
                "quote": r.get::<String, _>("quote"),
                "tick_size": r.get::<Decimal, _>("tick_size").to_string(),
                "lot_size": r.get::<Decimal, _>("lot_size").to_string(),
                "min_order_size": r.get::<Decimal, _>("min_order_size").to_string(),
                "max_order_size": r.get::<Decimal, _>("max_order_size").to_string(),
                "price_precision": r.get::<i16, _>("price_precision"),
                "qty_precision": r.get::<i16, _>("qty_precision"),
                "price_band_pct": r.get::<Decimal, _>("price_band_pct").to_string(),
                "active": r.get::<bool, _>("active"),
            })
        })
        .collect();

    let pairs_json = json!({"pairs": pairs});
    let pairs_str = serde_json::to_string(&pairs_json)?;
    cache::set_and_publish(redis, "cache:pairs", &pairs_str).await?;
    tracing::info!(count = pairs.len(), "cache:pairs seeded");
    Ok(())
}

// ── Order message format (same as gateway currently pushes to queue) ──────────

#[derive(Deserialize, Debug)]
struct QueuedOrderMsg {
    /// Order ID — generated by gateway. Optional for SQS-direct mode
    /// (API Gateway → SQS, no gateway Lambda). Worker generates UUID if missing.
    #[serde(default)]
    id: Option<String>,
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
    /// Set by gateway Lambda. In SQS-direct mode, worker uses current time.
    #[serde(default)]
    received_at: Option<String>,
    #[serde(default)]
    time_in_force: Option<String>,
}

fn default_gtc() -> String { "GTC".to_string() }
fn default_user_id() -> String { "user-1".to_string() }


// -- Public entry point

/// Process an order inline -- no cross-Lambda invoke.
/// Called from routes.rs when ORDER_DISPATCH_MODE=inline.
pub async fn process_order_inline(order_json: &serde_json::Value) -> Result<()> {
    let state = get_state().await?;
    process_order(state, order_json).await
}

/// Core order processing logic — extracted from sme-api worker.rs process_queued_order().
pub async fn process_order(state: &WorkerState, payload: &Value) -> Result<()> {
    let total_start = std::time::Instant::now();

    // Parse the order JSON from the Lambda event payload
    let msg: QueuedOrderMsg = serde_json::from_value(payload.clone())
        .context("failed to parse order payload")?;

    // Generate order ID if not provided (SQS-direct mode: no gateway Lambda)
    let order_id = match &msg.id {
        Some(id) if !id.is_empty() => Uuid::parse_str(id).context("invalid order id")?,
        _ => Uuid::new_v4(),
    };
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
    // Accept both "tif" and "time_in_force" field names (client may send either)
    let tif_str = msg.time_in_force.as_deref().unwrap_or(msg.tif.as_str());
    let tif: TimeInForce = match tif_str {
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

    // Basic validation (duplicated from gateway for SQS-direct mode where
    // orders bypass gateway Lambda and go straight from API GW → SQS → worker)
    if quantity <= Decimal::ZERO {
        anyhow::bail!("quantity must be positive");
    }
    if order_type == OrderType::Limit && price.is_none() {
        anyhow::bail!("limit orders require price");
    }
    if order_type == OrderType::Market && price.is_some() {
        anyhow::bail!("market orders cannot have price");
    }

    let created_at = msg.created_at.as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);

    let received_at = msg.received_at.as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(created_at);

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
        received_at: Some(received_at),
        matched_at: None,
        persisted_at: None,
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
        &state.redis,
        &order,
        pair_keys,
        &lock_asset,
        lock_amount_scaled,
    ).await.map_err(|e| {
        tracing::error!(error = %e, pair_id = %order.pair_id, "Lua EVAL failed");
        anyhow::anyhow!("Lua EVAL failed: {e}")
    })?;
    let lua_ms = match_start.elapsed().as_millis() as u64;
    order.matched_at = Some(Utc::now());

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
        .map_err(|e| { tracing::error!(error = %e, order_id = %order.id, "persist_order detail"); e })
        .context("DB persist failed")?;
    // persisted_at is set by PG NOW() in the INSERT — no extra roundtrip needed
    let persist_ms = persist_start.elapsed().as_millis() as u64;

    // 8. Update cache keys (orderbook, trades, ticker, portfolio)
    //    Non-fatal: order is already matched + persisted. Stale cache is acceptable —
    //    next successful order or EventBridge drain will refresh it.
    if let Err(e) = update_cache_after_processing(state, &order, &trades).await {
        tracing::warn!(
            error = %e,
            order_id = %order.id,
            pair_id = %order.pair_id,
            "cache update failed (non-fatal, order already persisted)"
        );
    }

    // 9. Push updates to WebSocket subscribers (fire-and-forget, non-critical)
    let ws_endpoint = std::env::var("WS_API_ENDPOINT").unwrap_or_default();
    if !ws_endpoint.is_empty() {
        push_ws_updates(&state.redis, &pair_id, &ws_endpoint).await;
    }

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

    // Insert the incoming order.
    // received_at and matched_at are Rust-side Utc::now() (Lambda clock).
    // persisted_at uses PG NOW() — same clock as other DB timestamps, no extra roundtrip.
    // Note: matched_at→persisted_at delta is all-PG-relative; received_at is Lambda-relative.
    // For E2E benchmark use matched_at→persisted_at (pure PG) + match latency from Rust.
    sqlx::query(
        "INSERT INTO orders (id, user_id, pair_id, side, order_type, tif, price, quantity, remaining, status, stp_mode, version, created_at, updated_at, client_order_id, received_at, matched_at, persisted_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,NOW())
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
    .bind(order.received_at)
    .bind(order.matched_at)
    .execute(&mut *tx)
    .await?;

    // Batch insert all trades and aggregate balance deltas
    let mut balance_deltas: HashMap<(String, String), (Decimal, Decimal)> = HashMap::new();

    if !trades.is_empty() {
        // Build batched trades INSERT using QueryBuilder
        let mut trade_query = QueryBuilder::<Postgres>::new(
            "INSERT INTO trades (id, pair_id, buy_order_id, sell_order_id, buyer_id, seller_id, price, quantity, created_at)"
        );
        trade_query.push_values(trades, |mut b, trade| {
            b.push_bind(trade.id)
             .push_bind(&trade.pair_id)
             .push_bind(trade.buy_order_id)
             .push_bind(trade.sell_order_id)
             .push_bind(&trade.buyer_id)
             .push_bind(&trade.seller_id)
             .push_bind(trade.price)
             .push_bind(trade.quantity)
             .push_bind(trade.created_at);
        });
        trade_query.push(" ON CONFLICT DO NOTHING");
        trade_query.build().execute(&mut *tx).await?;

        // Aggregate balance deltas
        for trade in trades {
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

    // Batch update resting orders after Lua fill
    let now = Utc::now();
    if !lua_trades.is_empty() {
        // Build batched resting order updates using CTE
        let mut updates_query = QueryBuilder::<Postgres>::new(
            "WITH updates(order_id, fill_qty) AS ("
        );
        updates_query.push_values(lua_trades, |mut b, lt| {
            b.push_bind(lt.resting_order_id)
             .push_bind(lt.quantity);
        });
        updates_query.push(") UPDATE orders o SET ")
                     .push("remaining = GREATEST(o.remaining - u.fill_qty, 0), ")
                     .push("status = CASE WHEN GREATEST(o.remaining - u.fill_qty, 0) = 0 THEN 'Filled' ELSE 'PartiallyFilled' END, ")
                     .push("version = o.version + 1, ")
                     .push("updated_at = ")
                     .push_bind(now)
                     .push(" FROM updates u WHERE o.id = u.order_id AND o.status IN ('New', 'PartiallyFilled')");
        updates_query.build().execute(&mut *tx).await?;
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

    // Batch audit events — fire-and-forget outside main transaction
    if !trades.is_empty() {
        // Build batch audit log INSERT for order + trades
        let mut audit_values = vec![
            (order.pair_id.clone(), "ORDER_CREATED".to_string(), json!({
                "order_id": order.id.to_string(),
                "user_id": order.user_id,
                "side": side_str(order.side),
                "order_type": order_type_str(order.order_type),
                "price": order.price.map(|v| v.to_string()),
                "quantity": order.quantity.to_string(),
            }))
        ];

        for trade in trades {
            audit_values.push((
                trade.pair_id.clone(),
                "TRADE_EXECUTED".to_string(),
                json!({
                    "trade_id": trade.id.to_string(),
                    "buy_order_id": trade.buy_order_id.to_string(),
                    "sell_order_id": trade.sell_order_id.to_string(),
                    "price": trade.price.to_string(),
                    "quantity": trade.quantity.to_string(),
                })
            ));
        }

        let mut audit_query = QueryBuilder::<Postgres>::new(
            "INSERT INTO audit_log (pair_id, event_type, payload)"
        );
        audit_query.push_values(&audit_values, |mut b, (pair_id, event_type, payload)| {
            b.push_bind(pair_id)
             .push_bind(event_type)
             .push_bind(payload);
        });

        let _ = audit_query.build().execute(&state.pg).await;
    } else {
        // Just the order creation audit event
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
    }

    Ok(())
}

// ── Cache updates ─────────────────────────────────────────────────────────────

/// Update Valkey cache keys after an order is processed.
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
    let (bids, asks) = cache::orderbook_snapshot_lua(&state.redis, &order.pair_id, 50).await
        .context("orderbook_snapshot_lua")?;

    let bids_json: Vec<Value> = bids.iter().map(|(p, q)| json!([p, q])).collect();
    let asks_json: Vec<Value> = asks.iter().map(|(p, q)| json!([p, q])).collect();

    let orderbook_json = serde_json::to_string(&json!({
        "pair": order.pair_id,
        "bids": bids_json,
        "asks": asks_json,
    }))?;
    cache::set_and_publish(&state.redis, &format!("cache:orderbook:{}", order.pair_id), &orderbook_json).await
        .context("set_and_publish orderbook")?;

    // Update trades cache from PG (last 50)
    update_trades_cache(state, &order.pair_id).await
        .context("update_trades_cache")?;

    // Update ticker cache from PG
    update_ticker_cache(state, &order.pair_id).await
        .context("update_ticker_cache")?;

    // Update portfolio caches for all affected users
    let mut dirty_users = std::collections::HashSet::new();
    dirty_users.insert(order.user_id.clone());
    for trade in trades {
        dirty_users.insert(trade.buyer_id.clone());
        dirty_users.insert(trade.seller_id.clone());
    }
    update_portfolio_caches(state, &dirty_users).await
        .context("update_portfolio_caches")?;

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
    cache::set_and_publish(&state.redis, &format!("cache:trades:{}", pair_id), &trades_str).await?;
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
    cache::set_and_publish(&state.redis, &format!("cache:ticker:{}", pair_id), &ticker_str).await?;
    Ok(())
}

async fn update_portfolio_caches(
    state: &WorkerState,
    user_ids: &std::collections::HashSet<String>,
) -> Result<()> {
    let mut conn = state.redis.get().await?;
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


// ── WebSocket push ────────────────────────────────────────────────────────────

pub async fn push_ws_updates(
    pool: &deadpool_redis::Pool,
    pair_id: &str,
    ws_endpoint: &str,
) {
    let mut conn = match pool.get().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "push_ws_updates: failed to get redis conn");
            return;
        }
    };

    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let apigw_config = aws_sdk_apigatewaymanagement::config::Builder::from(&config)
        .endpoint_url(ws_endpoint)
        .build();
    let apigw = aws_sdk_apigatewaymanagement::Client::from_conf(apigw_config);

    let channels = [
        format!("orderbook:{}", pair_id),
        format!("trades:{}", pair_id),
        format!("ticker:{}", pair_id),
    ];

    use deadpool_redis::redis::AsyncCommands;

    for ch_name in &channels {
        let subscribers: Vec<String> = match conn
            .smembers::<_, Vec<String>>(format!("ws:subs:{}", ch_name))
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(channel = %ch_name, error = %e, "push_ws_updates: smembers failed");
                continue;
            }
        };

        if subscribers.is_empty() { continue; }

        let cache_key = format!("cache:{}", ch_name);
        let val: String = match conn.get::<_, String>(&cache_key).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        if val.is_empty() { continue; }

        let msg = format!(r#"{{"ch":"{}","data":{}}}"#, ch_name, val);
        let msg_bytes = msg.into_bytes();
        let mut stale_conns: Vec<String> = vec![];

        for conn_id in &subscribers {
            match apigw
                .post_to_connection()
                .connection_id(conn_id)
                .data(aws_sdk_apigatewaymanagement::primitives::Blob::new(msg_bytes.clone()))
                .send()
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    let is_gone = e.as_service_error()
                        .map(|se| se.is_gone_exception())
                        .unwrap_or(false);
                    if is_gone {
                        tracing::info!(connection_id = %conn_id, "removing stale ws connection");
                        stale_conns.push(conn_id.clone());
                    } else {
                        tracing::warn!(connection_id = %conn_id, channel = %ch_name,
                            error = %e, "push_ws_updates: post_to_connection failed");
                    }
                }
            }
        }

        for stale_id in &stale_conns {
            let _ = conn.srem::<_, _, ()>("ws:connections", stale_id).await;
            let _ = conn.srem::<_, _, ()>(format!("ws:subs:{}", ch_name), stale_id).await;
            let subscribed_channels: Vec<String> = conn
                .smembers(format!("ws:conn:{}", stale_id))
                .await
                .unwrap_or_default();
            for other_ch in &subscribed_channels {
                let _ = conn.srem::<_, _, ()>(format!("ws:subs:{}", other_ch), stale_id).await;
            }
            let _ = conn.del::<_, ()>(format!("ws:conn:{}", stale_id)).await;
        }
    }
}
