//! Worker module — consumes orders from per-pair queues and updates cache keys.

use deadpool_redis::redis::AsyncCommands;
use serde_json::{json, Value};
use chrono::Utc;
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;
use sqlx::Row;
use tokio::sync::mpsc;

use sme_shared::{
    cache, metrics, Order, OrderStatus, OrderType, Side,
    SelfTradePreventionMode, TimeInForce, Trade,
};

use crate::{AppState, routes::{PersistJob, validate_order_request, lock_balance, order_to_json, trade_to_json}};

const PAIRS: &[&str] = &["BTC-USDT", "ETH-USDT", "SOL-USDT"];

// ── Batched metrics accumulator ──────────────────────────────────────────────
// Instead of 4 fire-and-forget Dragonfly writes per order, accumulate in-memory
// and flush periodically. Reduces Dragonfly ops from ~400/sec to ~3/sec at 100 ord/sec.

pub struct MetricsBatch {
    pub order_counts: [AtomicU64; 3],   // per pair index
    pub trade_counts: [AtomicU64; 3],
    pub latency_samples: [std::sync::Mutex<Vec<u64>>; 3], // lua_ms per pair
}

impl MetricsBatch {
    pub fn new() -> Self {
        Self {
            order_counts: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
            trade_counts: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
            latency_samples: [
                std::sync::Mutex::new(Vec::new()),
                std::sync::Mutex::new(Vec::new()),
                std::sync::Mutex::new(Vec::new()),
            ],
        }
    }

    fn pair_index(pair_id: &str) -> usize {
        match pair_id {
            "BTC-USDT" => 0,
            "ETH-USDT" => 1,
            "SOL-USDT" => 2,
            _ => 0,
        }
    }

    pub fn record_order(&self, pair_id: &str, trade_count: usize, lua_ms: u64) {
        let idx = Self::pair_index(pair_id);
        self.order_counts[idx].fetch_add(1, Ordering::Relaxed);
        if trade_count > 0 {
            self.trade_counts[idx].fetch_add(trade_count as u64, Ordering::Relaxed);
        }
        if let Ok(mut samples) = self.latency_samples[idx].lock() {
            samples.push(lua_ms);
            // Cap at 1000 samples to prevent unbounded growth between flushes
            if samples.len() > 1000 {
                samples.drain(..500);
            }
        }
    }

    /// Flush accumulated metrics to Dragonfly. Called every 2s by metrics flush worker.
    pub async fn flush(&self, pool: &deadpool_redis::Pool) {
        for (i, pair_id) in PAIRS.iter().enumerate() {
            let orders = self.order_counts[i].swap(0, Ordering::Relaxed);
            let trades = self.trade_counts[i].swap(0, Ordering::Relaxed);
            let samples: Vec<u64> = self.latency_samples[i].lock()
                .map(|mut s| s.drain(..).collect())
                .unwrap_or_default();

            if orders > 0 {
                let _ = metrics::increment_order_count_by(pool, pair_id, orders).await;
            }
            if trades > 0 {
                let _ = metrics::increment_trade_count(pool, pair_id, trades).await;
            }
            for ms in &samples {
                let _ = metrics::record_match_latency(pool, pair_id, *ms).await;
            }
        }
    }
}

// ── Orderbook rebuild debouncer ──────────────────────────────────────────────
// Instead of rebuilding the orderbook cache after every single order,
// coalesce rapid updates into one rebuild per pair per debounce window.

pub struct OrderbookDebouncer {
    /// Notify channels per pair — send () to signal "needs rebuild"
    notify: [tokio::sync::Notify; 3],
}

impl OrderbookDebouncer {
    pub fn new() -> Self {
        Self {
            notify: [
                tokio::sync::Notify::new(),
                tokio::sync::Notify::new(),
                tokio::sync::Notify::new(),
            ],
        }
    }

    /// Signal that a pair's orderbook needs rebuilding.
    pub fn mark_dirty(&self, pair_id: &str) {
        let idx = MetricsBatch::pair_index(pair_id);
        self.notify[idx].notify_one();
    }
}

/// Spawn debounced orderbook rebuild tasks — one per pair.
/// Waits for dirty signal, then coalesces for 50ms before rebuilding.
pub fn spawn_orderbook_debounce_workers(
    debouncer: Arc<OrderbookDebouncer>,
    state: AppState,
) {
    for (i, &pair_id) in PAIRS.iter().enumerate() {
        let debouncer = debouncer.clone();
        let state = state.clone();
        let pair_owned = pair_id.to_string();
        tokio::spawn(async move {
            loop {
                // Wait for at least one dirty signal
                debouncer.notify[i].notified().await;
                // Coalesce: wait 50ms for more updates to arrive
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;

                let start = std::time::Instant::now();
                if let Err(e) = rebuild_orderbook_cache(&state, &pair_owned).await {
                    tracing::warn!(error = %e, pair_id = %pair_owned, "debounced orderbook rebuild failed");
                } else {
                    let elapsed_us = start.elapsed().as_micros();
                    tracing::debug!(pair_id = %pair_owned, elapsed_us = elapsed_us, "orderbook cache rebuilt (debounced)");
                }
            }
        });
    }
}

/// Rebuild orderbook cache for a single pair — extracted from update_order_cache_after_processing.
async fn rebuild_orderbook_cache(state: &AppState, pair_id: &str) -> anyhow::Result<()> {
    let mut conn = state.dragonfly.get().await?;
    let bids = cache::load_order_book_batched(&state.dragonfly, pair_id, Side::Buy, 0, 50).await?;
    let asks = cache::load_order_book_batched(&state.dragonfly, pair_id, Side::Sell, 0, 50).await?;

    let bids_agg = aggregate_levels(bids);
    let asks_agg = aggregate_levels(asks);

    let orderbook_json = json!({
        "pair": pair_id,
        "bids": bids_agg,
        "asks": asks_agg,
    });
    let orderbook_str = serde_json::to_string(&orderbook_json)?;
    conn.set::<_, _, ()>(format!("cache:orderbook:{}", pair_id), &orderbook_str).await?;
    Ok(())
}

/// Main order queue consumer — spawns one task per pair plus a fallback task.
///
/// Per-pair tasks do BRPOP on `queue:orders:{pair_id}`.
/// Fallback task does BRPOP on `queue:orders` for backward-compat.
/// Cancellation consumers: one per pair on `queue:cancellations:{pair_id}` + fallback.
pub async fn order_queue_consumer(state: AppState) {
    tracing::info!("order queue consumer starting per-pair tasks");

    // Spawn one order consumer per pair
    for &pair in PAIRS {
        let s = state.clone();
        let pair_str = pair.to_string();
        tokio::spawn(async move {
            pair_order_consumer(s, pair_str).await;
        });
    }

    // Spawn one cancellation consumer per pair
    for &pair in PAIRS {
        let s = state.clone();
        let pair_str = pair.to_string();
        tokio::spawn(async move {
            pair_cancellation_consumer(s, pair_str).await;
        });
    }

    // Fallback cancellation consumer — handles queue:cancellations
    {
        let s = state.clone();
        tokio::spawn(async move {
            fallback_cancellation_consumer(s).await;
        });
    }

    // Fallback order consumer — handles legacy queue:orders (runs in this task)
    fallback_order_consumer(state).await;
}

/// BRPOP loop for a single pair's order queue: `queue:orders:{pair_id}`.
/// After each BRPOP wake, drains up to 9 more orders with non-blocking RPOP,
/// then spawns each order's processing concurrently (up to 10 in-flight per pair).
async fn pair_order_consumer(state: AppState, pair_id: String) {
    let queue_key = format!("queue:orders:{}", pair_id);
    tracing::info!(pair_id = %pair_id, queue = %queue_key, "pair order consumer started");

    // Semaphore: at most 3 orders in-flight concurrently per pair.
    // Higher values cause PG row-lock contention on balances and
    // Dragonfly Lua executor serialization under burst.
    let sem = Arc::new(tokio::sync::Semaphore::new(3));

    loop {
        let mut conn = match state.dragonfly.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, pair_id = %pair_id, "failed to get dragonfly connection");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        let result: Option<(String, String)> = match conn.brpop(&queue_key, 1.0).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "BRPOP timeout or error");
                None
            }
        };

        let first_order = match result {
            Some((_key, s)) => s,
            None => continue,
        };

        // Drain up to 2 more orders non-blocking (batch of 3 max)
        let mut batch = vec![first_order];
        for _ in 0..2 {
            let extra: Option<String> = conn.rpop(&queue_key, None).await.unwrap_or(None);
            match extra {
                Some(s) => batch.push(s),
                None => break,
            }
        }
        drop(conn); // release connection before spawning

        // Spawn each order concurrently, limited by semaphore
        for order_str in batch {
            let permit = sem.clone().acquire_owned().await.unwrap();
            let state_clone = state.clone();
            tokio::spawn(async move {
                if let Err(e) = process_queued_order(&state_clone, &order_str).await {
                    tracing::error!(error = %e, order_str = %order_str, "failed to process order");
                }
                drop(permit);
            });
        }
    }
}

/// BRPOP loop for a single pair's cancellation queue: `queue:cancellations:{pair_id}`.
async fn pair_cancellation_consumer(state: AppState, pair_id: String) {
    let queue_key = format!("queue:cancellations:{}", pair_id);
    tracing::info!(pair_id = %pair_id, queue = %queue_key, "pair cancellation consumer started");

    // Semaphore: at most 5 cancellations in-flight concurrently per pair
    let sem = Arc::new(tokio::sync::Semaphore::new(5));

    loop {
        let mut conn = match state.dragonfly.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "failed to get dragonfly connection");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        let result: Option<(String, String)> = match conn.brpop(&queue_key, 1.0).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "BRPOP cancellation timeout or error");
                None
            }
        };

        if let Some((_key, cancel_str)) = result {
            let permit = sem.clone().acquire_owned().await.unwrap();
            let state_clone = state.clone();
            tokio::spawn(async move {
                if let Err(e) = process_cancellation(&state_clone, &cancel_str).await {
                    tracing::error!(error = %e, cancel_str = %cancel_str, "failed to process cancellation");
                }
                drop(permit);
            });
        }
    }
}

/// Fallback BRPOP loop on `queue:orders` — backward compat for old producers.
async fn fallback_order_consumer(state: AppState) {
    tracing::info!("fallback order consumer started on queue:orders");

    // Semaphore: at most 3 orders in-flight concurrently
    let sem = Arc::new(tokio::sync::Semaphore::new(3));

    loop {
        let mut conn = match state.dragonfly.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "failed to get dragonfly connection");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        let result: Option<(String, String)> = match conn.brpop("queue:orders", 1.0).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "BRPOP timeout or error");
                None
            }
        };

        let first_order = match result {
            Some((_key, s)) => s,
            None => continue,
        };

        // Drain up to 9 more non-blocking
        let mut batch = vec![first_order];
        for _ in 0..9 {
            let extra: Option<String> = conn.rpop("queue:orders", None).await.unwrap_or(None);
            match extra {
                Some(s) => batch.push(s),
                None => break,
            }
        }
        drop(conn);

        for order_str in batch {
            let permit = sem.clone().acquire_owned().await.unwrap();
            let state_clone = state.clone();
            tokio::spawn(async move {
                if let Err(e) = process_queued_order(&state_clone, &order_str).await {
                    tracing::error!(error = %e, order_str = %order_str, "failed to process order");
                }
                drop(permit);
            });
        }
    }
}

/// Fallback BRPOP loop on `queue:cancellations` — catches cancellations from gateway
/// that don't know the pair_id.
async fn fallback_cancellation_consumer(state: AppState) {
    tracing::info!("fallback cancellation consumer started on queue:cancellations");

    // Semaphore: at most 5 cancellations in-flight concurrently
    let sem = Arc::new(tokio::sync::Semaphore::new(5));

    loop {
        let mut conn = match state.dragonfly.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "failed to get dragonfly connection");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        let result: Option<(String, String)> = match conn.brpop("queue:cancellations", 1.0).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "BRPOP cancellation timeout or error");
                None
            }
        };

        if let Some((_key, cancel_str)) = result {
            let permit = sem.clone().acquire_owned().await.unwrap();
            let state_clone = state.clone();
            tokio::spawn(async move {
                if let Err(e) = process_cancellation(&state_clone, &cancel_str).await {
                    tracing::error!(error = %e, cancel_str = %cancel_str, "failed to process cancellation");
                }
                drop(permit);
            });
        }
    }
}

/// Process a single order from the queue.
async fn process_queued_order(state: &AppState, order_str: &str) -> anyhow::Result<()> {
    let total_start = std::time::Instant::now();

    // Parse the order JSON from the gateway
    let parse_start = std::time::Instant::now();
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

    let parse_us = parse_start.elapsed().as_micros() as u64;

    // Measure queue-to-pickup latency (gateway created_at → now)
    let queue_latency_us = {
        let now_ts = Utc::now().timestamp_micros();
        let created_ts = order.created_at.timestamp_micros();
        (now_ts - created_ts).max(0) as u64
    };

    // 1. Validate (outside lock) — uses in-memory cache, zero DB round-trips
    let validate_start = std::time::Instant::now();
    validate_order_request(&state.pairs_cache, &order)
        .map_err(|e| anyhow::anyhow!("validation failed: {}", e))?;
    let validate_us = validate_start.elapsed().as_micros() as u64;

    // 2. Lock balance (pre-match) — insert_order_db deferred to persist worker
    let pre_lock_start = std::time::Instant::now();
    lock_balance(&state.pg, &order)
        .await
        .map_err(|e| anyhow::anyhow!("lock balance failed: {}", e))?;
    let db_pre_us = pre_lock_start.elapsed().as_micros() as u64;

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
    let lua_us = match_start.elapsed().as_micros() as u64;

    // Apply Lua result to the incoming order struct
    order.remaining = lua_result.remaining;
    order.status = lua_result.status;

    let trade_count = lua_result.trades.len();

    // Metrics — accumulate in-memory batch (flushed every 2s, zero Dragonfly ops here)
    state.metrics_batch.record_order(pair_id, trade_count, lua_us / 1000);

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

    // Notify dirty-user channel: order owner + counterparties need portfolio refresh
    let _ = state.dirty_users_tx.try_send(user_id.clone());
    for lt in &lua_result.trades {
        if lt.buyer_id != user_id {
            let _ = state.dirty_users_tx.try_send(lt.buyer_id.clone());
        }
        if lt.seller_id != user_id {
            let _ = state.dirty_users_tx.try_send(lt.seller_id.clone());
        }
    }

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

    // 5. Emit WS event synchronously (just a channel send — negligible latency)
    if !trade_jsons.is_empty() {
        let event_msg = serde_json::to_string(&json!({
            "type": "order_created",
            "user_id": order.user_id,
            "order": order_to_json(&order),
            "trades": &trade_jsons,
        })).unwrap_or_default();
        let _ = state.order_events_tx.send(event_msg);
    }

    // 5b. Signal orderbook rebuild via debouncer — coalesces rapid updates.
    // Only signals if the book actually changed (trade occurred or order rested).
    let order_rested = order.status == OrderStatus::New
        || order.status == OrderStatus::PartiallyFilled;
    let had_trades = !trade_jsons.is_empty();
    if had_trades || order_rested {
        state.orderbook_debouncer.mark_dirty(pair_id);
    }
    let cache_us = 0u64;

    let total_us = total_start.elapsed().as_micros() as u64;

    tracing::info!(
        queue_latency_us = queue_latency_us,
        parse_us = parse_us,
        validate_us = validate_us,
        db_pre_us = db_pre_us,
        lua_us = lua_us,
        cache_us = cache_us,
        total_us = total_us,
        trade_count = trade_count,
        pair_id = %order.pair_id,
        order_id = %order.id,
        "order processed [segments]"
    );

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

            // Mark user portfolio as dirty
            let _ = state.dirty_users_tx.try_send(user_id.to_string());

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
                // Try to route to per-pair queue if pair_id available
                let pair_id = new_order.get("pair_id").and_then(|v| v.as_str());
                let queue = pair_id.map(|p| format!("queue:orders:{}", p))
                    .unwrap_or_else(|| "queue:orders".to_string());
                conn.lpush::<_, _, ()>(queue, &order_str).await?;
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
    // Skip orderbook rebuild if there were no trades and the order didn't rest.
    // e.g. IOC that didn't match — no book mutation occurred.
    let order_rested = order.status == OrderStatus::New
        || order.status == OrderStatus::PartiallyFilled;
    let had_trades = !trade_jsons.is_empty();

    if !had_trades && !order_rested {
        // No book change — skip the reload entirely
        return Ok(());
    }

    let mut conn = state.dragonfly.get().await?;

    // Rebuild orderbook cache: load top-50 levels each side (UI shows ~20 levels)
    let bids = sme_shared::cache::load_order_book_batched(&state.dragonfly, &order.pair_id, Side::Buy, 0, 50).await?;
    let asks = sme_shared::cache::load_order_book_batched(&state.dragonfly, &order.pair_id, Side::Sell, 0, 50).await?;
    
    let bids_agg = aggregate_levels(bids);
    let asks_agg = aggregate_levels(asks);
    
    let orderbook_json = json!({
        "pair": order.pair_id,
        "bids": bids_agg,
        "asks": asks_agg,
    });
    let orderbook_str = serde_json::to_string(&orderbook_json)?;
    conn.set::<_, _, ()>(format!("cache:orderbook:{}", order.pair_id), &orderbook_str).await?;

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
///
/// Ticker/trades: polled every 5s (sub-second freshness not required).
/// Portfolio: only updates users marked dirty by order processing.
pub async fn cache_refresh_worker(state: AppState, mut dirty_users_rx: mpsc::Receiver<String>) {
    use tokio::time::{Duration, interval};
    
    tracing::info!("cache refresh worker started");
    
    // 5s interval — ticker/trades don't need sub-second freshness
    let mut ticker = interval(Duration::from_secs(5));

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

        // Drain dirty users and update only those portfolios
        let mut dirty: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Ok(uid) = dirty_users_rx.try_recv() {
            dirty.insert(uid);
        }

        if !dirty.is_empty() {
            if let Err(e) = update_portfolio_caches_for_users(&state, &dirty).await {
                tracing::warn!(error = %e, "failed to update portfolio caches");
            }
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

/// Update portfolio caches only for the given dirty user IDs.
async fn update_portfolio_caches_for_users(
    state: &AppState,
    user_ids: &std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    if user_ids.is_empty() {
        return Ok(());
    }

    let mut conn = state.dragonfly.get().await?;

    for user_id in user_ids {
        let rows = sqlx::query(
            "SELECT user_id, asset, available, locked FROM balances WHERE user_id = $1 ORDER BY asset",
        )
        .bind(user_id)
        .fetch_all(&state.pg_bg)
        .await?;

        let balances: Vec<Value> = rows.iter().map(|r| {
            json!({
                "user_id": r.get::<String, _>("user_id"),
                "asset": r.get::<String, _>("asset"),
                "available": r.get::<Decimal, _>("available").to_string(),
                "locked": r.get::<Decimal, _>("locked").to_string(),
            })
        }).collect();

        let portfolio_json = json!({"balances": balances});
        let portfolio_str = serde_json::to_string(&portfolio_json)?;
        conn.set::<_, _, ()>(format!("cache:portfolio:{}", user_id), &portfolio_str).await?;
    }

    Ok(())
}
