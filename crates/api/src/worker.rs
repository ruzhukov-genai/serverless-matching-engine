//! Worker module — consumes orders from queue:orders and updates cache keys.

use deadpool_redis::redis::AsyncCommands;
use serde_json::{json, Value};
use chrono::Utc;
use rust_decimal::Decimal;
use std::str::FromStr;
use uuid::Uuid;
use sqlx::Row;

use sme_shared::{
    cache, metrics, Order, OrderStatus, OrderType, Side,
    SelfTradePreventionMode, TimeInForce, Trade,
};

use crate::{AppState, routes::{PersistJob, validate_order_request, insert_order_db, lock_balance, order_to_json, trade_to_json}};

const PAIRS: &[&str] = &["BTC-USDT", "ETH-USDT", "SOL-USDT"];

/// Main order queue consumer — processes orders from queue:orders.
pub async fn order_queue_consumer(state: AppState) {
    tracing::info!("order queue consumer started");

    loop {
        let mut conn = match state.dragonfly.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "failed to get dragonfly connection");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        // BRPOP blocks until an order is available (1s timeout)
        let result: Option<(String, String)> = match conn.brpop("queue:orders", 1.0).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "BRPOP timeout or error");
                None
            }
        };
        if let Some((_key, order_str)) = result
            && let Err(e) = process_queued_order(&state, &order_str).await
        {
            tracing::error!(error = %e, order_str = %order_str, "failed to process order");
        }

        // Also try to process a cancellation (non-blocking via RPOP)
        let cancel: Option<String> = conn.rpop("queue:cancellations", None).await.unwrap_or(None);
        if let Some(cancel_str) = cancel
            && let Err(e) = process_cancellation(&state, &cancel_str).await
        {
            tracing::error!(error = %e, cancel_str = %cancel_str, "failed to process cancellation");
        }
    }
}

/// Process a single order from the queue.
async fn process_queued_order(state: &AppState, order_str: &str) -> anyhow::Result<()> {
    let total_start = std::time::Instant::now();

    // Parse the order JSON from the gateway
    let order_json: Value = serde_json::from_str(order_str)?;
    
    let user_id = order_json.get("user_id").and_then(|v| v.as_str()).unwrap_or("user-1").to_string();
    let pair_id = order_json.get("pair_id").and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("missing pair_id"))?;
    let side: Side = order_json.get("side").and_then(|v| v.as_str()).and_then(|s| match s {
        "Buy" => Some(Side::Buy),
        "Sell" => Some(Side::Sell),
        _ => None,
    }).ok_or_else(|| anyhow::anyhow!("invalid side"))?;
    
    let order_type: OrderType = order_json.get("order_type").and_then(|v| v.as_str()).and_then(|s| match s {
        "Market" => Some(OrderType::Market),
        "Limit" => Some(OrderType::Limit),
        _ => None,
    }).ok_or_else(|| anyhow::anyhow!("invalid order_type"))?;
    
    let tif: TimeInForce = order_json.get("tif").and_then(|v| v.as_str()).and_then(|s| match s {
        "IOC" => Some(TimeInForce::IOC),
        "FOK" => Some(TimeInForce::FOK),
        "GTC" => Some(TimeInForce::GTC),
        _ => None,
    }).unwrap_or(TimeInForce::GTC);
    
    let stp_mode: SelfTradePreventionMode = order_json.get("stp_mode").and_then(|v| v.as_str()).and_then(|s| match s {
        "CancelTaker" => Some(SelfTradePreventionMode::CancelTaker),
        "CancelMaker" => Some(SelfTradePreventionMode::CancelMaker),
        "CancelBoth" => Some(SelfTradePreventionMode::CancelBoth),
        "None" => Some(SelfTradePreventionMode::None),
        _ => None,
    }).unwrap_or(SelfTradePreventionMode::None);

    let price = order_json.get("price").and_then(|v| v.as_str()).and_then(|s| Decimal::from_str(s).ok());
    let quantity = order_json.get("quantity").and_then(|v| v.as_str()).and_then(|s| Decimal::from_str(s).ok())
        .ok_or_else(|| anyhow::anyhow!("invalid quantity"))?;

    let order_id_str = order_json.get("id").and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("missing order id"))?;
    let order_id = Uuid::parse_str(order_id_str)?;

    let client_order_id = order_json.get("client_order_id").and_then(|v| v.as_str()).map(|s| s.to_string());

    let created_at = order_json.get("created_at").and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(Utc::now);

    let now = Utc::now();
    let mut order = Order {
        id: order_id,
        user_id: user_id.clone(),
        pair_id: pair_id.to_string(),
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
        client_order_id,
    };

    // 1. Validate (outside lock) — uses in-memory cache, zero DB round-trips
    let pre_lock_start = std::time::Instant::now();
    validate_order_request(&state.pairs_cache, &order)
        .map_err(|e| anyhow::anyhow!("validation failed: {}", e))?;

    // 2. Insert to DB (outside lock — unique constraint enforces idempotency)
    insert_order_db(&state.pg, &order).await?;

    // 3. Lock balance in DB (outside lock)
    lock_balance(&state.pg, &order).await
        .map_err(|e| anyhow::anyhow!("lock balance failed: {}", e))?;
    
    let _pre_lock_ms = pre_lock_start.elapsed().as_millis() as u64;

    // 4. Match the order using Lua atomic EVAL
    let match_start = std::time::Instant::now();

    // For FOK orders, use the OCC retry loop like the original implementation
    if tif == TimeInForce::FOK {
        tracing::warn!("FOK orders not implemented in worker yet, cancelling");
        order.status = OrderStatus::Cancelled;
        update_order_cache_after_processing(state, &order, &[]).await?;
        return Ok(());
    }

    // Non-FOK: single atomic Lua EVAL — no lock, no retry
    let lua_result = cache::match_order_lua(&state.dragonfly, &order).await?;
    let lua_ms = match_start.elapsed().as_millis() as u64;

    // Apply Lua result to the incoming order struct
    order.remaining = lua_result.remaining;
    order.status = lua_result.status;

    let trade_count = lua_result.trades.len();

    // Metrics (best-effort)
    {
        let pool = state.dragonfly.clone();
        let pair_id = pair_id.to_string();
        tokio::spawn(async move {
            let _ = metrics::record_match_latency(&pool, &pair_id, lua_ms).await;
            let _ = metrics::record_lock_wait(&pool, &pair_id, 0).await;
            let _ = metrics::increment_order_count(&pool, &pair_id).await;
            if trade_count > 0 {
                let _ = metrics::increment_trade_count(&pool, &pair_id, trade_count as u64).await;
            }
        });
    }

    // Build Trade objects (UUIDs generated in Rust, not Lua)
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

    // Build trade JSON synchronously from Trade structs — no DB round-trip needed
    let trade_jsons: Vec<Value> = trades.iter().map(trade_to_json).collect();

    // Send persist job to background worker (non-blocking)
    let job = PersistJob {
        order: order.clone(),
        trades,
        lua_trades: lua_result.trades,
    };
    match state.persist_tx.try_send(job) {
        Ok(()) => {}
        Err(e) => {
            // Channel full or closed — bypass channel and spawn directly
            tracing::warn!("persist channel full, spawning direct persist task");
            let job = e.into_inner();
            let pg = state.pg_bg.clone();
            tokio::spawn(async move {
                if let Err(err) = crate::routes::process_persist_job(&pg, job).await {
                    tracing::error!(error = %err, "direct persist task failed");
                }
            });
        }
    }

    let total_ms = total_start.elapsed().as_millis() as u64;

    tracing::info!(
        lua_ms = lua_ms,
        total_ms = total_ms,
        trade_count = trade_count,
        pair_id = %order.pair_id,
        order_id = %order.id,
        "order processed"
    );

    // Update caches after successful processing
    update_order_cache_after_processing(state, &order, &trade_jsons).await?;

    Ok(())
}

/// Process a cancellation request from the queue.
async fn process_cancellation(state: &AppState, cancel_str: &str) -> anyhow::Result<()> {
    let cancel_json: Value = serde_json::from_str(cancel_str)?;
    let cancel_type = cancel_json.get("type").and_then(|v| v.as_str()).unwrap_or("cancel");

    match cancel_type {
        "cancel" => {
            let order_id_str = cancel_json.get("order_id").and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing order_id"))?;
            let order_id = Uuid::parse_str(order_id_str)?;
            let user_id = cancel_json.get("user_id").and_then(|v| v.as_str()).unwrap_or("");

            // 1. Get current order from DB
            let row = sqlx::query("SELECT pair_id, side, price, remaining, status FROM orders WHERE id = $1 AND user_id = $2")
                .bind(order_id).bind(user_id)
                .fetch_optional(&state.pg).await?;

            let row = match row {
                Some(r) => r,
                None => { tracing::warn!(%order_id, "cancel: order not found"); return Ok(()); }
            };

            let status: String = row.get("status");
            if status != "New" && status != "PartiallyFilled" {
                tracing::warn!(%order_id, %status, "cancel: order not cancellable");
                return Ok(());
            }

            let pair_id: String = row.get("pair_id");
            let side: String = row.get("side");
            let price: Option<Decimal> = row.get("price");
            let remaining: Decimal = row.get("remaining");

            // 2. Remove from Dragonfly book
            let side_enum = if side == "Buy" { Side::Buy } else { Side::Sell };
            let cancel_order = Order {
                id: order_id,
                user_id: user_id.to_string(),
                pair_id: pair_id.clone(),
                side: side_enum,
                order_type: OrderType::Limit,
                tif: TimeInForce::GTC,
                price,
                quantity: remaining,
                remaining,
                status: OrderStatus::Cancelled,
                stp_mode: SelfTradePreventionMode::None,
                version: 1, sequence: 0,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                client_order_id: None,
            };
            cache::remove_order_from_book(&state.dragonfly, &cancel_order).await?;

            // 3. Update DB status
            sqlx::query("UPDATE orders SET status = 'Cancelled', remaining = 0, updated_at = NOW() WHERE id = $1")
                .bind(order_id).execute(&state.pg).await?;

            // 4. Release locked balance
            let (asset, amount) = if side == "Buy" {
                ("USDT", remaining * price.unwrap_or(Decimal::ZERO))
            } else {
                (pair_id.split('-').next().unwrap_or("BTC"), remaining)
            };
            sqlx::query("UPDATE balances SET locked = locked - $1, available = available + $1 WHERE user_id = $2 AND asset = $3")
                .bind(amount).bind(user_id).bind(asset).execute(&state.pg_bg).await?;

            tracing::info!(%order_id, "order cancelled");
        }
        "modify" => {
            // Cancel-and-replace: cancel the old, queue a new order
            let order_id_str = cancel_json.get("order_id").and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing order_id"))?;
            
            // Cancel the existing order first
            let cancel_msg = json!({"type": "cancel", "order_id": order_id_str,
                "user_id": cancel_json.get("user_id").and_then(|v| v.as_str()).unwrap_or("")}).to_string();
            Box::pin(process_cancellation(state, &cancel_msg)).await?;

            // Queue the replacement order if new_order data provided
            if let Some(new_order) = cancel_json.get("new_order") {
                let mut conn = state.dragonfly.get().await?;
                let order_str = serde_json::to_string(new_order)?;
                conn.lpush::<_, _, ()>("queue:orders", &order_str).await?;
                tracing::info!(old_order_id = %order_id_str, "modify: replacement order queued");
            }
        }
        "cancel_all" => {
            let user_id = cancel_json.get("user_id").and_then(|v| v.as_str()).unwrap_or("user-1");
            let pair_id = cancel_json.get("pair_id").and_then(|v| v.as_str());

            // Get all open orders for user
            let query = if let Some(pair) = pair_id {
                sqlx::query("SELECT id, pair_id, side, price, remaining FROM orders WHERE user_id = $1 AND pair_id = $2 AND status IN ('New','PartiallyFilled')")
                    .bind(user_id).bind(pair).fetch_all(&state.pg).await?
            } else {
                sqlx::query("SELECT id, pair_id, side, price, remaining FROM orders WHERE user_id = $1 AND status IN ('New','PartiallyFilled')")
                    .bind(user_id).fetch_all(&state.pg).await?
            };

            let count = query.len();
            for row in &query {
                let oid: Uuid = row.get("id");
                let cancel_msg = json!({"type": "cancel", "order_id": oid.to_string(), "user_id": user_id}).to_string();
                if let Err(e) = Box::pin(process_cancellation(state, &cancel_msg)).await {
                    tracing::warn!(error = %e, order_id = %oid, "cancel_all: failed to cancel order");
                }
            }
            tracing::info!(%user_id, pair_id = ?pair_id, count, "cancel_all processed");
        }
        _ => {
            tracing::warn!(%cancel_type, "unknown cancellation type");
        }
    }

    Ok(())
}

/// Update cache keys after processing an order.
async fn update_order_cache_after_processing(
    state: &AppState,
    order: &Order,
    trade_jsons: &[Value],
) -> anyhow::Result<()> {
    let mut conn = state.dragonfly.get().await?;

    // Update cache:orderbook:{pair} - refresh from Dragonfly orderbook data
    let bids = sme_shared::cache::load_order_book_batched(&state.dragonfly, &order.pair_id, Side::Buy, 0, 200).await?;
    let asks = sme_shared::cache::load_order_book_batched(&state.dragonfly, &order.pair_id, Side::Sell, 0, 200).await?;
    
    let bids_agg = aggregate_levels(bids);
    let asks_agg = aggregate_levels(asks);
    
    let orderbook_json = json!({
        "pair": order.pair_id,
        "bids": bids_agg,
        "asks": asks_agg,
    });
    let orderbook_str = serde_json::to_string(&orderbook_json)?;
    conn.set::<_, _, ()>(format!("cache:orderbook:{}", order.pair_id), &orderbook_str).await?;

    // Emit order event to broadcast channel
    if !trade_jsons.is_empty() {
        let event_msg = serde_json::to_string(&json!({
            "type": "order_created",
            "user_id": order.user_id,
            "order": order_to_json(order),
            "trades": trade_jsons,
        })).unwrap_or_default();
        let _ = state.order_events_tx.send(event_msg);
    }

    Ok(())
}

fn aggregate_levels(orders: Vec<Order>) -> Vec<Value> {
    let mut map: std::collections::BTreeMap<String, Decimal> = std::collections::BTreeMap::new();
    for o in &orders {
        if let Some(p) = o.price {
            *map.entry(p.to_string()).or_insert(Decimal::ZERO) += o.remaining;
        }
    }
    map.into_iter()
        .map(|(price, qty)| json!({ "price": price, "quantity": qty.to_string() }))
        .collect()
}

/// Background cache refresh worker — updates ticker, trades, portfolio, metrics.
pub async fn cache_refresh_worker(state: AppState) {
    use tokio::time::{Duration, interval};
    
    tracing::info!("cache refresh worker started");
    
    let mut ticker = interval(Duration::from_secs(2));

    loop {
        ticker.tick().await;

        // Update ticker caches for all pairs
        if let Err(e) = update_ticker_caches(&state).await {
            tracing::warn!(error = %e, "failed to update ticker caches");
        }

        // Update trades caches for all pairs
        if let Err(e) = update_trades_caches(&state).await {
            tracing::warn!(error = %e, "failed to update trades caches");
        }

        // Update portfolio caches for all users
        if let Err(e) = update_portfolio_caches(&state).await {
            tracing::warn!(error = %e, "failed to update portfolio caches");
        }
    }
}

async fn update_ticker_caches(state: &AppState) -> anyhow::Result<()> {
    let mut conn = state.dragonfly.get().await?;

    for pair_id in PAIRS {
        // Query latest ticker data from DB
        let ticker_row = sqlx::query(
            "SELECT 
                MAX(price) FILTER (WHERE created_at >= NOW() - INTERVAL '24 hours') as high_24h,
                MIN(price) FILTER (WHERE created_at >= NOW() - INTERVAL '24 hours') as low_24h,
                SUM(quantity) FILTER (WHERE created_at >= NOW() - INTERVAL '24 hours') as volume_24h
             FROM trades WHERE pair_id = $1",
        )
        .bind(pair_id)
        .fetch_optional(&state.pg_bg)
        .await?;

        let last_row = sqlx::query(
            "SELECT price as last_price FROM trades WHERE pair_id = $1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(pair_id)
        .fetch_optional(&state.pg_bg)
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
            json!({
                "pair": pair_id,
                "last": last,
                "high_24h": null,
                "low_24h": null,
                "volume_24h": null,
            })
        };

        let ticker_str = serde_json::to_string(&ticker_json)?;
        conn.set::<_, _, ()>(format!("cache:ticker:{}", pair_id), &ticker_str).await?;
    }

    Ok(())
}

async fn update_trades_caches(state: &AppState) -> anyhow::Result<()> {
    let mut conn = state.dragonfly.get().await?;

    for pair_id in PAIRS {
        let rows = sqlx::query(
            "SELECT id, pair_id, buy_order_id, sell_order_id, buyer_id, seller_id, price, quantity, sequence, created_at
             FROM trades WHERE pair_id = $1 ORDER BY created_at DESC LIMIT 50",
        )
        .bind(pair_id)
        .fetch_all(&state.pg_bg)
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

        let trades_json = json!({
            "pair": pair_id,
            "trades": trades
        });
        
        let trades_str = serde_json::to_string(&trades_json)?;
        conn.set::<_, _, ()>(format!("cache:trades:{}", pair_id), &trades_str).await?;
    }

    Ok(())
}

async fn update_portfolio_caches(state: &AppState) -> anyhow::Result<()> {
    let mut conn = state.dragonfly.get().await?;

    // Get all users with balances
    let rows = sqlx::query(
        "SELECT user_id, asset, available, locked FROM balances ORDER BY user_id, asset",
    )
    .fetch_all(&state.pg_bg)
    .await?;

    let mut by_user: std::collections::HashMap<String, Vec<Value>> = std::collections::HashMap::new();
    for r in &rows {
        let uid: String = r.get("user_id");
        by_user.entry(uid).or_default().push(json!({
            "user_id": r.get::<String, _>("user_id"),
            "asset": r.get::<String, _>("asset"),
            "available": r.get::<Decimal, _>("available").to_string(),
            "locked": r.get::<Decimal, _>("locked").to_string(),
        }));
    }

    for (user_id, balances) in by_user {
        let portfolio_json = json!({"balances": balances});
        let portfolio_str = serde_json::to_string(&portfolio_json)?;
        conn.set::<_, _, ()>(format!("cache:portfolio:{}", user_id), &portfolio_str).await?;
    }

    Ok(())
}