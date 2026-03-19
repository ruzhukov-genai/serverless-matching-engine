//! Worker-side route helpers — persistence, validation, cache refresh.
//! HTTP handlers live in the gateway crate.

use std::collections::HashMap;

use tokio::sync::mpsc;

use chrono::Utc;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use sme_shared::{
    cache, metrics, Order, OrderStatus, OrderType, Side,
    SelfTradePreventionMode, TimeInForce, Trade,
};

use crate::PairConfig;

const PAIRS: &[&str] = &["BTC-USDT", "ETH-USDT", "SOL-USDT"];

// ── Async persistence ─────────────────────────────────────────────────────────

/// Job sent to the background persistence worker after a Lua match completes.
pub struct PersistJob {
    pub order: Order,
    pub trades: Vec<Trade>,
    pub lua_trades: Vec<cache::LuaTrade>,
}

/// Spawn a background worker that drains `rx`, batches up to 50 jobs, and
/// persists them all in a single PostgreSQL transaction.
pub fn spawn_persist_worker(
    pg: sqlx::PgPool,
    mut rx: mpsc::Receiver<PersistJob>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(first_job) = rx.recv().await {
            let mut batch = vec![first_job];
            // Drain more without blocking (up to 50 total)
            while batch.len() < 50 {
                match rx.try_recv() {
                    Ok(job) => batch.push(job),
                    Err(_) => break,
                }
            }
            let count = batch.len();
            if let Err(e) = process_persist_batch(&pg, batch).await {
                tracing::error!(error = %e, batch_size = count, "batch persist failed");
            }
        }
    })
}

/// Execute all DB writes for a single matched order (off the hot path).
pub async fn process_persist_job(pg: &sqlx::PgPool, job: PersistJob) -> anyhow::Result<()> {
    let persist_start = std::time::Instant::now();

    // 0. Insert order to DB (deferred from hot path)
    insert_order_db(pg, &job.order).await?;

    // 1. Audit: ORDER_CREATED
    insert_audit_event(
        pg,
        Some(&job.order.pair_id),
        "ORDER_CREATED",
        &json!({
            "order_id": job.order.id.to_string(),
            "user_id": job.order.user_id,
            "side": side_str(job.order.side),
            "order_type": order_type_str(job.order.order_type),
            "price": job.order.price.map(|v| v.to_string()),
            "quantity": job.order.quantity.to_string(),
        }),
    )
    .await
    .ok();

    // 2. Persist trades + settle balances (single DB transaction)
    let _ = persist_trades(pg, &job.trades).await?;

    // 3. Update resting orders' statuses in DB
    update_resting_orders_after_lua(pg, &job.lua_trades).await?;

    // 4. Update incoming order's remaining + status in DB
    update_order_db(pg, &job.order).await?;

    // 5. Release balance lock if order is fully resolved
    if job.order.status == OrderStatus::Cancelled || job.order.status == OrderStatus::Filled {
        release_remaining_locked(pg, &job.order).await?;
    }

    let persist_ms = persist_start.elapsed().as_millis() as u64;
    tracing::info!(
        persist_ms = persist_ms,
        trade_count = job.trades.len(),
        order_id = %job.order.id,
        "persist complete"
    );

    Ok(())
}


/// Execute all DB writes for a batch of matched orders in ONE transaction.
/// Called by the batching persist worker; keeps `process_persist_job` intact
/// for the fallback direct-spawn path.
async fn process_persist_batch(pg: &sqlx::PgPool, batch: Vec<PersistJob>) -> anyhow::Result<()> {
    let persist_start = std::time::Instant::now();
    let batch_size = batch.len();

    let mut tx = pg.begin().await?;

    // 1. INSERT all orders
    for job in &batch {
        sqlx::query(
            "INSERT INTO orders (id, user_id, pair_id, side, order_type, tif, price, quantity, remaining, status, stp_mode, version, created_at, updated_at, client_order_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
             ON CONFLICT DO NOTHING",
        )
        .bind(job.order.id)
        .bind(&job.order.user_id)
        .bind(&job.order.pair_id)
        .bind(side_str(job.order.side))
        .bind(order_type_str(job.order.order_type))
        .bind(tif_str(job.order.tif))
        .bind(job.order.price)
        .bind(job.order.quantity)
        .bind(job.order.remaining)
        .bind(status_str(job.order.status))
        .bind(stp_str(job.order.stp_mode))
        .bind(job.order.version)
        .bind(job.order.created_at)
        .bind(job.order.updated_at)
        .bind(&job.order.client_order_id)
        .execute(&mut *tx)
        .await?;
    }

    // 2. INSERT all trades + aggregate balance deltas across ALL jobs
    let mut balance_deltas: HashMap<(String, String), (Decimal, Decimal)> = HashMap::new();

    for job in &batch {
        for trade in &job.trades {
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

            let (base, quote) = sme_shared::parse_pair_id(&trade.pair_id)?;
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

    // 3. Apply one UPDATE per (user_id, asset) — sorted to prevent deadlocks
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
        .bind(user_id)
        .bind(asset)
        .execute(&mut *tx)
        .await?;
    }

    // 4. UPDATE resting orders
    let now = Utc::now();
    for job in &batch {
        for lt in &job.lua_trades {
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
    }

    // 5. UPDATE incoming orders (remaining + status)
    for job in &batch {
        sqlx::query(
            "UPDATE orders SET remaining = $1, status = $2, version = version + 1, updated_at = $3 WHERE id = $4",
        )
        .bind(job.order.remaining)
        .bind(status_str(job.order.status))
        .bind(now)
        .bind(job.order.id)
        .execute(&mut *tx)
        .await?;
    }

    // 6. Release remaining locked for fully-resolved orders
    for job in &batch {
        if (job.order.status == OrderStatus::Cancelled || job.order.status == OrderStatus::Filled)
            && job.order.remaining != Decimal::ZERO
        {
            let (base, quote) = sme_shared::parse_pair_id(&job.order.pair_id)?;
            let (asset, amount) = match job.order.side {
                Side::Buy  => {
                    let price = job.order.price.unwrap_or(Decimal::ZERO);
                    (quote, price * job.order.remaining)
                }
                Side::Sell => (base, job.order.remaining),
            };
            sqlx::query(
                "UPDATE balances SET available = available + $1, locked = GREATEST(locked - $1, 0) WHERE user_id = $2 AND asset = $3",
            )
            .bind(amount)
            .bind(&job.order.user_id)
            .bind(&asset)
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;

    // 7. Audit events — fire-and-forget outside main transaction
    for job in &batch {
        sqlx::query("INSERT INTO audit_log (pair_id, event_type, payload) VALUES ($1, $2, $3)")
            .bind(&job.order.pair_id)
            .bind("ORDER_CREATED")
            .bind(json!({
                "order_id": job.order.id.to_string(),
                "user_id": job.order.user_id,
                "side": side_str(job.order.side),
                "order_type": order_type_str(job.order.order_type),
                "price": job.order.price.map(|v| v.to_string()),
                "quantity": job.order.quantity.to_string(),
            }))
            .execute(pg)
            .await
            .ok();

        for trade in &job.trades {
            sqlx::query("INSERT INTO audit_log (pair_id, event_type, payload) VALUES ($1, $2, $3)")
                .bind(&trade.pair_id)
                .bind("TRADE_EXECUTED")
                .bind(json!({
                    "trade_id": trade.id.to_string(),
                    "buy_order_id": trade.buy_order_id.to_string(),
                    "sell_order_id": trade.sell_order_id.to_string(),
                    "price": trade.price.to_string(),
                    "quantity": trade.quantity.to_string(),
                }))
                .execute(pg)
                .await
                .ok();
        }
    }

    let persist_ms = persist_start.elapsed().as_millis() as u64;
    tracing::info!(
        persist_ms = persist_ms,
        batch_size = batch_size,
        "batch persist complete"
    );

    Ok(())
}

pub async fn insert_audit_event(
    pg: &sqlx::PgPool,
    pair_id: Option<&str>,
    event_type: &str,
    payload: &serde_json::Value,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO audit_log (pair_id, event_type, payload) VALUES ($1, $2, $3)",
    )
    .bind(pair_id)
    .bind(event_type)
    .bind(payload)
    .execute(pg)
    .await?;
    Ok(())
}

// ── DB helpers ─────────────────────────────────────────────────────────────────

/// Validate order against the in-memory pairs cache — zero DB round-trips.
pub fn validate_order_request(
    pairs_cache: &HashMap<String, PairConfig>,
    order: &Order,
) -> anyhow::Result<()> {
    let cfg = pairs_cache
        .get(&order.pair_id)
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
    if let Some(price) = order.price
        && cfg.tick_size > Decimal::ZERO
        && price % cfg.tick_size != Decimal::ZERO
    {
        anyhow::bail!("price not aligned to tick_size");
    }
    Ok(())
}

pub async fn insert_order_db(pg: &sqlx::PgPool, order: &Order) -> anyhow::Result<()> {
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
    .execute(pg)
    .await?;
    Ok(())
}

async fn update_order_db(pg: &sqlx::PgPool, order: &Order) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE orders SET remaining = $1, status = $2, version = version + 1, updated_at = $3 WHERE id = $4",
    )
    .bind(order.remaining)
    .bind(status_str(order.status))
    .bind(Utc::now())
    .bind(order.id)
    .execute(pg)
    .await?;
    Ok(())
}

/// Batch update multiple orders in a single transaction.
/// Replaces the per-order loop (N separate round-trips) with 1 transaction.
async fn _batch_update_orders_db(pg: &sqlx::PgPool, orders: &[&Order]) -> anyhow::Result<()> {
    if orders.is_empty() {
        return Ok(());
    }
    let now = Utc::now();
    let mut tx = pg.begin().await?;
    for order in orders {
        sqlx::query(
            "UPDATE orders SET remaining = $1, status = $2, version = version + 1, updated_at = $3 WHERE id = $4",
        )
        .bind(order.remaining)
        .bind(format!("{:?}", order.status))
        .bind(now)
        .bind(order.id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn _cancel_order_in_db(pg: &sqlx::PgPool, order_id: Uuid) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE orders SET status = 'Cancelled', updated_at = NOW() WHERE id = $1",
    )
    .bind(order_id)
    .execute(pg)
    .await?;
    Ok(())
}

async fn _load_order_db(pg: &sqlx::PgPool, order_id: Uuid) -> anyhow::Result<Option<Order>> {
    let row = sqlx::query(
        "SELECT id, user_id, pair_id, side, order_type, tif, price, quantity, remaining, status, stp_mode, version, sequence, created_at, updated_at, client_order_id
         FROM orders WHERE id = $1",
    )
    .bind(order_id)
    .fetch_optional(pg)
    .await?;

    Ok(row.map(|r| _row_to_order(&r)))
}



fn _row_to_order(r: &sqlx::postgres::PgRow) -> Order {
    let side: Side = match r.get::<String, _>("side").as_str() {
        "Buy" => Side::Buy,
        _ => Side::Sell,
    };
    let order_type: OrderType = match r.get::<String, _>("order_type").as_str() {
        "Market" => OrderType::Market,
        _ => OrderType::Limit,
    };
    let tif: TimeInForce = match r.get::<String, _>("tif").as_str() {
        "IOC" => TimeInForce::IOC,
        "FOK" => TimeInForce::FOK,
        _ => TimeInForce::GTC,
    };
    let status: OrderStatus = match r.get::<String, _>("status").as_str() {
        "PartiallyFilled" => OrderStatus::PartiallyFilled,
        "Filled" => OrderStatus::Filled,
        "Cancelled" => OrderStatus::Cancelled,
        "Rejected" => OrderStatus::Rejected,
        _ => OrderStatus::New,
    };
    let stp_mode: SelfTradePreventionMode = match r.get::<String, _>("stp_mode").as_str() {
        "CancelTaker" => SelfTradePreventionMode::CancelTaker,
        "CancelMaker" => SelfTradePreventionMode::CancelMaker,
        "CancelBoth" => SelfTradePreventionMode::CancelBoth,
        _ => SelfTradePreventionMode::None,
    };

    Order {
        id: r.get("id"),
        user_id: r.get("user_id"),
        pair_id: r.get("pair_id"),
        side,
        order_type,
        tif,
        price: r.get("price"),
        quantity: r.get("quantity"),
        remaining: r.get("remaining"),
        status,
        stp_mode,
        version: r.get("version"),
        sequence: r.get::<Option<i64>, _>("sequence").unwrap_or(0),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        client_order_id: r.get("client_order_id"),
    }
}

fn _row_to_order_json(r: &sqlx::postgres::PgRow) -> Value {
    let order = _row_to_order(r);
    order_to_json(&order)
}

// ── Static string helpers for enum → &str (avoid format!("{:?}") allocations) ─

pub fn side_str(s: Side) -> &'static str {
    match s { Side::Buy => "Buy", Side::Sell => "Sell" }
}

pub fn order_type_str(ot: OrderType) -> &'static str {
    match ot { OrderType::Limit => "Limit", OrderType::Market => "Market" }
}

pub fn tif_str(tif: TimeInForce) -> &'static str {
    match tif { TimeInForce::GTC => "GTC", TimeInForce::IOC => "IOC", TimeInForce::FOK => "FOK" }
}

pub fn status_str(s: OrderStatus) -> &'static str {
    match s {
        OrderStatus::New             => "New",
        OrderStatus::PartiallyFilled => "PartiallyFilled",
        OrderStatus::Filled          => "Filled",
        OrderStatus::Cancelled       => "Cancelled",
        OrderStatus::Rejected        => "Rejected",
    }
}

pub fn stp_str(s: SelfTradePreventionMode) -> &'static str {
    match s {
        SelfTradePreventionMode::None         => "None",
        SelfTradePreventionMode::CancelMaker  => "CancelMaker",
        SelfTradePreventionMode::CancelTaker  => "CancelTaker",
        SelfTradePreventionMode::CancelBoth   => "CancelBoth",
    }
}

pub fn order_to_json(o: &Order) -> Value {
    let mut j = json!({
        "id": o.id.to_string(),
        "user_id": o.user_id,
        "pair_id": o.pair_id,
        "side": side_str(o.side),
        "order_type": order_type_str(o.order_type),
        "tif": tif_str(o.tif),
        "price": o.price.map(|v| v.to_string()),
        "quantity": o.quantity.to_string(),
        "remaining": o.remaining.to_string(),
        "status": status_str(o.status),
        "stp_mode": stp_str(o.stp_mode),
        "version": o.version,
        "created_at": o.created_at.to_rfc3339(),
        "updated_at": o.updated_at.to_rfc3339(),
    });
    if let Some(ref coid) = o.client_order_id {
        j["client_order_id"] = json!(coid);
    }
    j
}

pub fn trade_to_json(t: &Trade) -> Value {
    json!({
        "id": t.id.to_string(),
        "pair_id": t.pair_id,
        "buy_order_id": t.buy_order_id.to_string(),
        "sell_order_id": t.sell_order_id.to_string(),
        "buyer_id": t.buyer_id,
        "seller_id": t.seller_id,
        "price": t.price.to_string(),
        "quantity": t.quantity.to_string(),
        "created_at": t.created_at.to_rfc3339(),
    })
}

#[allow(dead_code)]
pub async fn lock_balance(pg: &sqlx::PgPool, order: &Order) -> anyhow::Result<()> {
    let (asset, amount) = get_lock_asset_amount(pg, order).await?;
    let rows_affected = sqlx::query(
        "UPDATE balances SET available = available - $1, locked = locked + $1
         WHERE user_id = $2 AND asset = $3 AND available >= $1",
    )
    .bind(amount)
    .bind(&order.user_id)
    .bind(&asset)
    .execute(pg)
    .await?
    .rows_affected();

    if rows_affected == 0 {
        anyhow::bail!("insufficient {} balance for user {}", asset, order.user_id);
    }
    Ok(())
}

async fn _release_locked_balance(pg: &sqlx::PgPool, order: &Order) -> anyhow::Result<()> {
    let (asset, amount) = get_lock_asset_amount(pg, order).await?;
    sqlx::query(
        "UPDATE balances SET available = available + $1, locked = GREATEST(locked - $1, 0)
         WHERE user_id = $2 AND asset = $3",
    )
    .bind(amount)
    .bind(&order.user_id)
    .bind(&asset)
    .execute(pg)
    .await?;
    Ok(())
}

async fn release_remaining_locked(pg: &sqlx::PgPool, order: &Order) -> anyhow::Result<()> {
    if order.remaining == Decimal::ZERO {
        return Ok(());
    }
    let (base, quote) = sme_shared::parse_pair_id(&order.pair_id)?;

    let (asset, amount) = match order.side {
        Side::Buy => {
            let price = order.price.unwrap_or(Decimal::ZERO);
            (quote, price * order.remaining)
        }
        Side::Sell => (base, order.remaining),
    };

    sqlx::query(
        "UPDATE balances SET available = available + $1, locked = GREATEST(locked - $1, 0)
         WHERE user_id = $2 AND asset = $3",
    )
    .bind(amount)
    .bind(&order.user_id)
    .bind(&asset)
    .execute(pg)
    .await?;
    Ok(())
}

#[allow(dead_code)]
async fn get_lock_asset_amount(
    _pg: &sqlx::PgPool,
    order: &Order,
) -> anyhow::Result<(String, Decimal)> {
    let (base, quote) = sme_shared::parse_pair_id(&order.pair_id)?;

    Ok(match order.side {
        Side::Buy => {
            let price = order.price.unwrap_or(Decimal::ZERO);
            (quote, price * order.quantity)
        }
        Side::Sell => (base, order.quantity),
    })
}

// cancel_order_with_lock removed — cancellation now handled via worker queue

/// Persist all trades from a single match in one DB transaction.
///
/// Opens one transaction, bulk-inserts all trades, aggregates balance deltas
/// into one UPDATE per (user_id, asset), batch-inserts audit events, then commits.
/// Returns a Vec<Value> of trade JSON objects.
async fn persist_trades(pg: &sqlx::PgPool, trades: &[Trade]) -> anyhow::Result<Vec<Value>> {
    if trades.is_empty() {
        return Ok(Vec::new());
    }

    let mut tx = pg.begin().await?;
    let mut trade_jsons = Vec::with_capacity(trades.len());

    // Step 1: INSERT all trades
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

        trade_jsons.push(trade_to_json(trade));
    }

    // Step 2: Aggregate balance deltas across all trades
    // Key: (user_id, asset) → (locked_decrease, available_increase)
    let mut deltas: HashMap<(String, String), (Decimal, Decimal)> = HashMap::new();

    for trade in trades {
        let (base, quote) = sme_shared::parse_pair_id(&trade.pair_id)?;
        let cost = trade.price * trade.quantity;

        // buyer (quote asset): locked -= cost
        deltas
            .entry((trade.buyer_id.clone(), quote.clone()))
            .or_insert((Decimal::ZERO, Decimal::ZERO))
            .0 += cost;

        // buyer (base asset): available += qty
        deltas
            .entry((trade.buyer_id.clone(), base.clone()))
            .or_insert((Decimal::ZERO, Decimal::ZERO))
            .1 += trade.quantity;

        // seller (base asset): locked -= qty
        deltas
            .entry((trade.seller_id.clone(), base.clone()))
            .or_insert((Decimal::ZERO, Decimal::ZERO))
            .0 += trade.quantity;

        // seller (quote asset): available += cost
        deltas
            .entry((trade.seller_id.clone(), quote.clone()))
            .or_insert((Decimal::ZERO, Decimal::ZERO))
            .1 += cost;
    }

    // Step 3: Apply one UPDATE per (user_id, asset).
    // Uses INSERT ON CONFLICT to handle new balance rows (e.g. buyer receiving
    // base asset for the first time).
    // Sort deltas by (user_id, asset) so every concurrent transaction acquires
    // row locks in the same order — eliminates ABBA deadlock.
    let mut sorted_deltas: Vec<_> = deltas.iter().collect();
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
        .bind(user_id)
        .bind(asset)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    // Step 4: Batch INSERT audit events (fire-and-forget outside transaction;
    // the audit_log.sequence column has no DEFAULT so inserts may fail silently
    // — matching original behaviour).
    for trade in trades {
        sqlx::query(
            "INSERT INTO audit_log (pair_id, event_type, payload) VALUES ($1, $2, $3)",
        )
        .bind(&trade.pair_id)
        .bind("TRADE_EXECUTED")
        .bind(json!({
            "trade_id": trade.id.to_string(),
            "buy_order_id": trade.buy_order_id.to_string(),
            "sell_order_id": trade.sell_order_id.to_string(),
            "price": trade.price.to_string(),
            "quantity": trade.quantity.to_string(),
        }))
        .execute(pg)
        .await
        .ok();
    }

    Ok(trade_jsons)
}

/// Update resting orders in Postgres after a Lua atomic fill.
///
/// Uses arithmetic SQL so we don't need the current remaining value.
/// Dragonfly is already correct; this syncs the authoritative Postgres state.
/// Runs each update individually (resting orders per match are typically ≤ a few).
async fn update_resting_orders_after_lua(
    pg: &sqlx::PgPool,
    lua_trades: &[cache::LuaTrade],
) -> anyhow::Result<()> {
    if lua_trades.is_empty() {
        return Ok(());
    }
    let now = Utc::now();
    let mut tx = pg.begin().await?;
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
    tx.commit().await?;
    Ok(())
}

#[allow(dead_code)]
async fn insert_trade_db(pg: &sqlx::PgPool, trade: &Trade) -> anyhow::Result<()> {
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
    .execute(pg)
    .await?;
    Ok(())
}

#[allow(dead_code)]
async fn settle_trade_balances(pg: &sqlx::PgPool, trade: &Trade) -> anyhow::Result<()> {
    let (base, quote) = sme_shared::parse_pair_id(&trade.pair_id)?;
    let cost = trade.price * trade.quantity;

    let mut tx = pg.begin().await?;

    sqlx::query("UPDATE balances SET locked = GREATEST(locked - $1, 0) WHERE user_id = $2 AND asset = $3")
        .bind(cost).bind(&trade.buyer_id).bind(&quote)
        .execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO balances (user_id, asset, available, locked) VALUES ($1,$2,$3,0)
         ON CONFLICT (user_id, asset) DO UPDATE SET available = balances.available + $3",
    )
    .bind(&trade.buyer_id).bind(&base).bind(trade.quantity)
    .execute(&mut *tx).await?;

    sqlx::query("UPDATE balances SET locked = GREATEST(locked - $1, 0) WHERE user_id = $2 AND asset = $3")
        .bind(trade.quantity).bind(&trade.seller_id).bind(&base)
        .execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO balances (user_id, asset, available, locked) VALUES ($1,$2,$3,0)
         ON CONFLICT (user_id, asset) DO UPDATE SET available = balances.available + $3",
    )
    .bind(&trade.seller_id).bind(&quote).bind(cost)
    .execute(&mut *tx).await?;

    tx.commit().await?;
    Ok(())
}

fn _sort_score(order: &Order) -> f64 {
    order.book_score()
}

// portfolio_refresh_loop_worker REMOVED — was duplicating cache_refresh_worker's
// dirty-user portfolio refresh AND scanning all users every 2s regardless of activity.
// Portfolio refresh now handled exclusively by cache_refresh_worker (dirty-users only).
#[allow(dead_code)]
fn row_to_trade(r: &sqlx::postgres::PgRow) -> Trade {
    Trade {
        id: r.get("id"),
        pair_id: r.get("pair_id"),
        buy_order_id: r.get("buy_order_id"),
        sell_order_id: r.get("sell_order_id"),
        buyer_id: r.get("buyer_id"),
        seller_id: r.get("seller_id"),
        price: r.get("price"),
        quantity: r.get("quantity"),
        sequence: r.get::<Option<i64>, _>("sequence").unwrap_or(0),
        created_at: r.get("created_at"),
    }
}

#[allow(dead_code)]
fn row_to_trade_json(r: &sqlx::postgres::PgRow) -> Value {
    let trade = row_to_trade(r);
    trade_to_json(&trade)
}

// ticker_trades_refresh_loop_worker REMOVED — was duplicating cache_refresh_worker's
// ticker + trades refresh at 2s interval. Now handled exclusively by cache_refresh_worker (5s).

/// Runs forever, refreshing ticker + recent trades for all pairs every 2 seconds (original in-memory version).
pub async fn metrics_refresh_loop_worker(
    dragonfly: deadpool_redis::Pool,
    pg: sqlx::PgPool,
) {
    use tokio::time::{Duration, interval};
    let mut ticker = interval(Duration::from_secs(5));

    loop {
        ticker.tick().await;

        // ── /api/metrics ──
        let mut total_orders: i64 = 0;
        let mut total_trades: i64 = 0;
        for pair_id in PAIRS {
            total_orders += metrics::get_order_count(&dragonfly, pair_id).await.unwrap_or(0);
            total_trades += metrics::get_trade_count(&dragonfly, pair_id).await.unwrap_or(0);
        }
        let mut all_latency: Vec<u64> = Vec::new();
        for pair_id in PAIRS {
            let samples = metrics::get_latency_samples(&dragonfly, pair_id, 100).await.unwrap_or_default();
            all_latency.extend(samples);
        }
        let (p50, p95, p99) = metrics::compute_percentiles(all_latency);

        let metrics_val = json!({
            "orders_per_sec": total_orders,
            "matches_per_sec": total_trades,
            "trades_per_sec": total_trades,
            "active_pairs": 3,
            "active_workers": 1,
            "latency": { "p50": p50, "p95": p95, "p99": p99 },
            "streams": {},
        });

        // ── /api/metrics/locks ──
        let mut all_waits: Vec<u64> = Vec::new();
        for pair_id in PAIRS {
            let samples = metrics::get_lock_wait_samples(&dragonfly, pair_id, 1000).await.unwrap_or_default();
            all_waits.extend(samples);
        }
        let (avg_wait_ms, contention_rate) = if all_waits.is_empty() {
            (0.0f64, 0.0f64)
        } else {
            let avg = all_waits.iter().sum::<u64>() as f64 / all_waits.len() as f64;
            let contended = all_waits.iter().filter(|&&v| v > 1).count();
            let rate = contended as f64 / all_waits.len() as f64;
            (avg, rate)
        };
        let locks_val = json!({
            "contention_rate": contention_rate,
            "avg_wait_ms": avg_wait_ms,
            "retry_count": all_waits.len(),
            "failures": 0,
        });

        // ── /api/metrics/latency (detailed) ──
        let mut all_samples: Vec<u64> = Vec::new();
        let mut per_pair: Vec<Value> = Vec::new();
        for pair_id in PAIRS {
            let samples = metrics::get_latency_samples(&dragonfly, pair_id, 1000).await.unwrap_or_default();
            let (pp50, pp95, pp99) = metrics::compute_percentiles(samples.clone());
            per_pair.push(json!({
                "pair_id": pair_id,
                "sample_count": samples.len(),
                "p50": pp50, "p95": pp95, "p99": pp99,
            }));
            all_samples.extend(samples);
        }
        let (lp50, lp95, lp99) = metrics::compute_percentiles(all_samples.clone());
        let latency_val = json!({
            "sample_count": all_samples.len(),
            "p50": lp50, "p95": lp95, "p99": lp99,
            "per_pair": per_pair,
        });

        // ── /api/metrics/throughput ──
        let throughput_val = match sqlx::query(
            "SELECT date_trunc('minute', created_at) as bucket, COUNT(*) as count
             FROM trades WHERE created_at >= NOW() - INTERVAL '1 hour'
             GROUP BY bucket ORDER BY bucket",
        )
        .fetch_all(&pg)
        .await {
            Ok(rows) => {
                let series: Vec<Value> = rows.iter().map(|r| {
                    json!({
                        "time": r.get::<chrono::DateTime<chrono::Utc>, _>("bucket").to_rfc3339(),
                        "count": r.get::<i64, _>("count"),
                    })
                }).collect();
                json!({ "series": series })
            }
            Err(_) => json!({ "series": [] }),
        };

        // Write to Dragonfly cache keys + PUBLISH for gateway subscribers
        let metrics_str = serde_json::to_string(&metrics_val).unwrap_or("{}".to_string());
        let _ = sme_shared::cache::set_and_publish(&dragonfly, "cache:metrics", &metrics_str).await;
        
        let locks_str = serde_json::to_string(&locks_val).unwrap_or("{}".to_string());
        let _ = sme_shared::cache::set_and_publish(&dragonfly, "cache:lock_metrics", &locks_str).await;
        
        let latency_str = serde_json::to_string(&latency_val).unwrap_or("{}".to_string());
        let _ = sme_shared::cache::set_and_publish(&dragonfly, "cache:latency_metrics", &latency_str).await;
        
        let throughput_str = serde_json::to_string(&throughput_val).unwrap_or("{}".to_string());
        let _ = sme_shared::cache::set_and_publish(&dragonfly, "cache:throughput", &throughput_str).await;
    }
}

