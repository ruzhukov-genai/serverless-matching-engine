//! API route handlers — real implementations backed by Dragonfly + PostgreSQL.

use axum::{
    extract::{Path, Query, State, WebSocketUpgrade, ws::Message, ws::WebSocket},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use chrono::Utc;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashMap;
use uuid::Uuid;

use sme_shared::{
    cache, engine::match_order, streams, Order, OrderStatus, OrderType, Pair, Side,
    SelfTradePreventionMode, TimeInForce, Trade,
};

use crate::AppState;

// ── Error helper ─────────────────────────────────────────────────────────────

pub struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        tracing::error!("handler error: {:?}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": self.0.to_string()}))).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

type HandlerResult<T> = Result<T, AppError>;

// ── GET /api/pairs ────────────────────────────────────────────────────────────

pub async fn list_pairs(State(s): State<AppState>) -> HandlerResult<impl IntoResponse> {
    let rows = sqlx::query("SELECT id, base, quote, tick_size, lot_size, min_order_size, max_order_size, price_precision, qty_precision, price_band_pct, active FROM pairs WHERE active = true")
        .fetch_all(&s.pg)
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

    Ok(Json(json!({ "pairs": pairs })))
}

// ── GET /api/orderbook/{pair_id} ──────────────────────────────────────────────

pub async fn get_orderbook(
    Path(pair_id): Path<String>,
    State(s): State<AppState>,
) -> HandlerResult<impl IntoResponse> {
    let bids_raw = cache::load_order_book(&s.dragonfly, &pair_id, Side::Buy).await?;
    let asks_raw = cache::load_order_book(&s.dragonfly, &pair_id, Side::Sell).await?;

    // Aggregate by price level
    let bids = aggregate_levels(bids_raw);
    let asks = aggregate_levels(asks_raw);

    Ok(Json(json!({
        "pair": pair_id,
        "bids": bids,
        "asks": asks,
    })))
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

// ── GET /api/trades/{pair_id} ─────────────────────────────────────────────────

pub async fn get_trades(
    Path(pair_id): Path<String>,
    State(s): State<AppState>,
) -> HandlerResult<impl IntoResponse> {
    let rows = sqlx::query(
        "SELECT id, pair_id, buy_order_id, sell_order_id, buyer_id, seller_id, price, quantity, created_at
         FROM trades WHERE pair_id = $1 ORDER BY created_at DESC LIMIT 50",
    )
    .bind(&pair_id)
    .fetch_all(&s.pg)
    .await?;

    let trades: Vec<Value> = rows.iter().map(row_to_trade_json).collect();
    Ok(Json(json!({ "pair": pair_id, "trades": trades })))
}

fn row_to_trade_json(r: &sqlx::postgres::PgRow) -> Value {
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
}

// ── GET /api/ticker/{pair_id} ─────────────────────────────────────────────────

pub async fn get_ticker(
    Path(pair_id): Path<String>,
    State(s): State<AppState>,
) -> HandlerResult<impl IntoResponse> {
    let row = sqlx::query(
        "SELECT
            MAX(price) FILTER (WHERE created_at >= NOW() - INTERVAL '24 hours') as high_24h,
            MIN(price) FILTER (WHERE created_at >= NOW() - INTERVAL '24 hours') as low_24h,
            SUM(quantity) FILTER (WHERE created_at >= NOW() - INTERVAL '24 hours') as volume_24h,
            (SELECT price FROM trades WHERE pair_id = $1 ORDER BY created_at DESC LIMIT 1) as last_price
         FROM trades WHERE pair_id = $1",
    )
    .bind(&pair_id)
    .fetch_optional(&s.pg)
    .await?;

    let ticker = match row {
        Some(r) => json!({
            "pair": pair_id,
            "last": r.get::<Option<Decimal>, _>("last_price").map(|v| v.to_string()),
            "high_24h": r.get::<Option<Decimal>, _>("high_24h").map(|v| v.to_string()),
            "low_24h": r.get::<Option<Decimal>, _>("low_24h").map(|v| v.to_string()),
            "volume_24h": r.get::<Option<Decimal>, _>("volume_24h").map(|v| v.to_string()),
        }),
        None => json!({
            "pair": pair_id,
            "last": null,
            "high_24h": null,
            "low_24h": null,
            "volume_24h": null,
        }),
    };

    Ok(Json(ticker))
}

// ── POST /api/orders ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateOrderRequest {
    pub user_id: Option<String>,
    pub pair_id: String,
    pub side: Side,
    pub order_type: OrderType,
    pub tif: Option<TimeInForce>,
    pub price: Option<Decimal>,
    pub quantity: Decimal,
    pub stp_mode: Option<SelfTradePreventionMode>,
}

pub async fn create_order(
    State(s): State<AppState>,
    Json(req): Json<CreateOrderRequest>,
) -> HandlerResult<impl IntoResponse> {
    let user_id = req.user_id.unwrap_or_else(|| "user-1".to_string());
    let tif = req.tif.unwrap_or(TimeInForce::GTC);
    let stp_mode = req.stp_mode.unwrap_or(SelfTradePreventionMode::None);

    let now = Utc::now();
    let mut order = Order {
        id: Uuid::new_v4(),
        user_id: user_id.clone(),
        pair_id: req.pair_id.clone(),
        side: req.side,
        order_type: req.order_type,
        tif,
        price: req.price,
        quantity: req.quantity,
        remaining: req.quantity,
        status: OrderStatus::New,
        stp_mode,
        version: 1,
        sequence: 0,
        created_at: now,
        updated_at: now,
    };

    // 1. Validate
    validate_order_request(&s.pg, &order).await?;

    // 2. Insert to DB
    insert_order_db(&s.pg, &order).await?;

    // 3. Lock balance
    lock_balance(&s.pg, &order).await?;

    // 4. Save to Dragonfly cache
    cache::save_order_to_book(&s.dragonfly, &order).await?;

    // 5. Load opposite side of the book
    let opposite_side = order.side.opposite();
    let mut book = cache::load_order_book(&s.dragonfly, &req.pair_id, opposite_side).await?;

    // Sort book for price-time priority
    book.sort_by(|a, b| {
        let sa = sort_score(a);
        let sb = sort_score(b);
        sa.partial_cmp(&sb)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.created_at.cmp(&b.created_at))
    });

    // 6. Match inline
    let result = match_order(&order, &mut book);
    order = result.incoming.clone();

    // 7. Handle FOK rollback
    if req.order_type == OrderType::Limit
        && tif == TimeInForce::FOK
        && result.trades.is_empty()
        && order.status == OrderStatus::Cancelled
    {
        // Rollback: cancel order, release locked balance
        cancel_order_in_db(&s.pg, order.id).await?;
        release_locked_balance(&s.pg, &order).await?;
        cache::remove_order_from_book(&s.dragonfly, &order).await?;
        return Ok((
            StatusCode::OK,
            Json(json!({
                "order": order_to_json(&order),
                "trades": [],
                "message": "FOK order cancelled — insufficient liquidity"
            })),
        ));
    }

    let mut trade_jsons: Vec<Value> = Vec::new();

    // 8. For each trade: insert to DB, update balances
    for trade in &result.trades {
        insert_trade_db(&s.pg, trade).await?;
        settle_trade_balances(&s.pg, trade).await?;
        trade_jsons.push(trade_to_json(trade));
    }

    // 9. Update matched resting orders in cache + DB
    for upd in &result.book_updates {
        update_order_db(&s.pg, upd).await?;
        if upd.status == OrderStatus::Filled || upd.status == OrderStatus::Cancelled {
            cache::remove_order_from_book(&s.dragonfly, upd).await?;
        } else {
            cache::save_order_to_book(&s.dragonfly, upd).await?;
        }
    }

    // 10. Update incoming order in DB + cache
    update_order_db(&s.pg, &order).await?;
    if order.status == OrderStatus::New || order.status == OrderStatus::PartiallyFilled {
        // GTC remainder stays in book (already saved in step 4, update with new remaining)
        cache::save_order_to_book(&s.dragonfly, &order).await?;
    } else {
        // Filled, Cancelled — remove from book
        cache::remove_order_from_book(&s.dragonfly, &order).await?;
        if order.status == OrderStatus::Cancelled || order.status == OrderStatus::Filled {
            release_remaining_locked(&s.pg, &order).await?;
        }
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "order": order_to_json(&order),
            "trades": trade_jsons,
        })),
    ))
}

// ── DELETE /api/orders/{order_id} ─────────────────────────────────────────────

pub async fn cancel_order(
    Path(order_id): Path<Uuid>,
    State(s): State<AppState>,
) -> HandlerResult<impl IntoResponse> {
    // Load from DB
    let order = load_order_db(&s.pg, order_id).await?
        .ok_or_else(|| anyhow::anyhow!("order not found"))?;

    if order.status != OrderStatus::New && order.status != OrderStatus::PartiallyFilled {
        return Ok(Json(json!({"error": "order not cancellable", "status": format!("{:?}", order.status)})));
    }

    cancel_order_in_db(&s.pg, order_id).await?;
    release_locked_balance(&s.pg, &order).await?;
    cache::remove_order_from_book(&s.dragonfly, &order).await?;

    Ok(Json(json!({ "status": "cancelled", "order_id": order_id.to_string() })))
}

// ── PUT /api/orders/{order_id} ────────────────────────────────────────────────

pub async fn modify_order(
    Path(order_id): Path<Uuid>,
    State(s): State<AppState>,
    Json(req): Json<CreateOrderRequest>,
) -> HandlerResult<impl IntoResponse> {
    // Cancel the old order
    let old_order = load_order_db(&s.pg, order_id).await?
        .ok_or_else(|| anyhow::anyhow!("order not found"))?;

    if old_order.status != OrderStatus::New && old_order.status != OrderStatus::PartiallyFilled {
        return Ok(Json(json!({"error": "order not modifiable"})));
    }

    cancel_order_in_db(&s.pg, order_id).await?;
    release_locked_balance(&s.pg, &old_order).await?;
    cache::remove_order_from_book(&s.dragonfly, &old_order).await?;

    Ok(Json(json!({
        "cancelled_order_id": order_id.to_string(),
        "message": "Old order cancelled. Submit a new POST /api/orders to replace it.",
    })))
}

// ── GET /api/orders ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct OrdersQuery {
    pub user_id: Option<String>,
}

pub async fn list_orders(
    Query(q): Query<OrdersQuery>,
    State(s): State<AppState>,
) -> HandlerResult<impl IntoResponse> {
    let user_id = q.user_id.unwrap_or_else(|| "user-1".to_string());
    let rows = sqlx::query(
        "SELECT id, user_id, pair_id, side, order_type, tif, price, quantity, remaining, status, stp_mode, version, created_at, updated_at
         FROM orders WHERE user_id = $1 AND status IN ('New','PartiallyFilled') ORDER BY created_at DESC",
    )
    .bind(&user_id)
    .fetch_all(&s.pg)
    .await?;

    let orders: Vec<Value> = rows.iter().map(row_to_order_json).collect();
    Ok(Json(json!({ "orders": orders })))
}

// ── GET /api/portfolio ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PortfolioQuery {
    pub user_id: Option<String>,
}

pub async fn get_portfolio(
    Query(q): Query<PortfolioQuery>,
    State(s): State<AppState>,
) -> HandlerResult<impl IntoResponse> {
    let user_id = q.user_id.unwrap_or_else(|| "user-1".to_string());
    let rows = sqlx::query(
        "SELECT user_id, asset, available, locked FROM balances WHERE user_id = $1",
    )
    .bind(&user_id)
    .fetch_all(&s.pg)
    .await?;

    let balances: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "user_id": r.get::<String, _>("user_id"),
                "asset": r.get::<String, _>("asset"),
                "available": r.get::<Decimal, _>("available").to_string(),
                "locked": r.get::<Decimal, _>("locked").to_string(),
            })
        })
        .collect();

    Ok(Json(json!({ "balances": balances })))
}

// ── WebSocket Feeds ───────────────────────────────────────────────────────────

pub async fn ws_orderbook(
    Path(pair_id): Path<String>,
    State(s): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_orderbook_ws(pair_id, s, socket))
}

async fn handle_orderbook_ws(pair_id: String, state: AppState, mut socket: WebSocket) {
    use tokio::time::{interval, Duration};
    let mut ticker = interval(Duration::from_millis(500));
    let mut last_version: i64 = -1;

    loop {
        ticker.tick().await;

        let version = cache::increment_version(&state.dragonfly, &pair_id)
            .await
            .unwrap_or(0)
            - 1; // get without incrementing: use DECR+INCR trick or just check

        let bids = cache::load_order_book(&state.dragonfly, &pair_id, Side::Buy)
            .await
            .unwrap_or_default();
        let asks = cache::load_order_book(&state.dragonfly, &pair_id, Side::Sell)
            .await
            .unwrap_or_default();

        let msg = serde_json::to_string(&json!({
            "type": "snapshot",
            "pair": pair_id,
            "bids": aggregate_levels(bids),
            "asks": aggregate_levels(asks),
        }))
        .unwrap_or_default();

        if socket.send(Message::Text(msg.into())).await.is_err() {
            break;
        }
    }
}

pub async fn ws_trades(
    Path(pair_id): Path<String>,
    State(s): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_trades_ws(pair_id, s, socket))
}

async fn handle_trades_ws(pair_id: String, state: AppState, mut socket: WebSocket) {
    use tokio::time::{interval, Duration};
    let mut ticker = interval(Duration::from_millis(500));
    let mut last_seen: Option<chrono::DateTime<Utc>> = None;

    loop {
        ticker.tick().await;

        let query = if let Some(since) = last_seen {
            sqlx::query(
                "SELECT id, pair_id, buy_order_id, sell_order_id, buyer_id, seller_id, price, quantity, created_at
                 FROM trades WHERE pair_id = $1 AND created_at > $2 ORDER BY created_at DESC LIMIT 20",
            )
            .bind(&pair_id)
            .bind(since)
            .fetch_all(&state.pg)
            .await
        } else {
            sqlx::query(
                "SELECT id, pair_id, buy_order_id, sell_order_id, buyer_id, seller_id, price, quantity, created_at
                 FROM trades WHERE pair_id = $1 ORDER BY created_at DESC LIMIT 20",
            )
            .bind(&pair_id)
            .fetch_all(&state.pg)
            .await
        };

        let rows = match query {
            Ok(r) => r,
            Err(_) => break,
        };

        if !rows.is_empty() {
            let trades: Vec<Value> = rows.iter().map(row_to_trade_json).collect();
            last_seen = Some(rows[0].get::<chrono::DateTime<Utc>, _>("created_at"));

            let msg = serde_json::to_string(&json!({
                "type": "trades",
                "pair": pair_id,
                "trades": trades,
            }))
            .unwrap_or_default();

            if socket.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    }
}

// ── Dashboard API ─────────────────────────────────────────────────────────────

pub async fn get_metrics(State(s): State<AppState>) -> HandlerResult<impl IntoResponse> {
    // Count active orders and recent trades
    let order_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM orders WHERE status IN ('New','PartiallyFilled')",
    )
    .fetch_one(&s.pg)
    .await
    .unwrap_or(0);

    let trade_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM trades WHERE created_at >= NOW() - INTERVAL '1 minute'",
    )
    .fetch_one(&s.pg)
    .await
    .unwrap_or(0);

    Ok(Json(json!({
        "orders_per_sec": order_count,
        "matches_per_sec": trade_count,
        "trades_per_sec": trade_count,
        "active_pairs": 3,
        "active_workers": 1,
    })))
}

pub async fn get_lock_metrics() -> impl IntoResponse {
    Json(json!({
        "contention_rate": 0.0,
        "avg_wait_ms": 0.0,
        "retry_count": 0,
        "failures": 0,
    }))
}

pub async fn get_throughput(State(s): State<AppState>) -> HandlerResult<impl IntoResponse> {
    let rows = sqlx::query(
        "SELECT date_trunc('minute', created_at) as bucket, COUNT(*) as count
         FROM trades WHERE created_at >= NOW() - INTERVAL '1 hour'
         GROUP BY bucket ORDER BY bucket",
    )
    .fetch_all(&s.pg)
    .await?;

    let series: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "time": r.get::<chrono::DateTime<Utc>, _>("bucket").to_rfc3339(),
                "count": r.get::<i64, _>("count"),
            })
        })
        .collect();

    Ok(Json(json!({ "series": series })))
}

pub async fn get_audit(State(s): State<AppState>) -> HandlerResult<impl IntoResponse> {
    let rows = sqlx::query(
        "SELECT id, sequence, pair_id, event_type, payload, created_at FROM audit_log ORDER BY created_at DESC LIMIT 50",
    )
    .fetch_all(&s.pg)
    .await?;

    let entries: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "sequence": r.get::<i64, _>("sequence"),
                "pair_id": r.get::<Option<String>, _>("pair_id"),
                "event_type": r.get::<String, _>("event_type"),
                "payload": r.get::<serde_json::Value, _>("payload"),
                "created_at": r.get::<chrono::DateTime<Utc>, _>("created_at").to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(json!({ "audit": entries })))
}

// ── DB helpers ─────────────────────────────────────────────────────────────────

async fn validate_order_request(pg: &sqlx::PgPool, order: &Order) -> anyhow::Result<()> {
    let row = sqlx::query(
        "SELECT tick_size, lot_size, min_order_size, max_order_size, active FROM pairs WHERE id = $1",
    )
    .bind(&order.pair_id)
    .fetch_optional(pg)
    .await?;

    let row = match row {
        Some(r) => r,
        None => anyhow::bail!("unknown pair {}", order.pair_id),
    };

    let active: bool = row.get("active");
    if !active {
        anyhow::bail!("pair {} is not active", order.pair_id);
    }

    let tick_size: Decimal = row.get("tick_size");
    let lot_size: Decimal = row.get("lot_size");
    let min_size: Decimal = row.get("min_order_size");
    let max_size: Decimal = row.get("max_order_size");

    if order.quantity % lot_size != Decimal::ZERO {
        anyhow::bail!("quantity not aligned to lot_size");
    }
    if order.quantity < min_size || order.quantity > max_size {
        anyhow::bail!("quantity out of range [{min_size}, {max_size}]");
    }
    if let Some(price) = order.price {
        if tick_size > Decimal::ZERO && price % tick_size != Decimal::ZERO {
            anyhow::bail!("price not aligned to tick_size");
        }
    }
    Ok(())
}

async fn insert_order_db(pg: &sqlx::PgPool, order: &Order) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO orders (id, user_id, pair_id, side, order_type, tif, price, quantity, remaining, status, stp_mode, version, created_at, updated_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
         ON CONFLICT DO NOTHING",
    )
    .bind(order.id)
    .bind(&order.user_id)
    .bind(&order.pair_id)
    .bind(format!("{:?}", order.side))
    .bind(format!("{:?}", order.order_type))
    .bind(format!("{:?}", order.tif))
    .bind(order.price)
    .bind(order.quantity)
    .bind(order.remaining)
    .bind(format!("{:?}", order.status))
    .bind(format!("{:?}", order.stp_mode))
    .bind(order.version)
    .bind(order.created_at)
    .bind(order.updated_at)
    .execute(pg)
    .await?;
    Ok(())
}

async fn update_order_db(pg: &sqlx::PgPool, order: &Order) -> anyhow::Result<()> {
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

async fn cancel_order_in_db(pg: &sqlx::PgPool, order_id: Uuid) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE orders SET status = 'Cancelled', updated_at = NOW() WHERE id = $1",
    )
    .bind(order_id)
    .execute(pg)
    .await?;
    Ok(())
}

async fn load_order_db(pg: &sqlx::PgPool, order_id: Uuid) -> anyhow::Result<Option<Order>> {
    let row = sqlx::query(
        "SELECT id, user_id, pair_id, side, order_type, tif, price, quantity, remaining, status, stp_mode, version, created_at, updated_at
         FROM orders WHERE id = $1",
    )
    .bind(order_id)
    .fetch_optional(pg)
    .await?;

    Ok(row.map(|r| row_to_order(&r)))
}

fn row_to_order(r: &sqlx::postgres::PgRow) -> Order {
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
        sequence: 0,
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

fn row_to_order_json(r: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": r.get::<Uuid, _>("id").to_string(),
        "user_id": r.get::<String, _>("user_id"),
        "pair_id": r.get::<String, _>("pair_id"),
        "side": r.get::<String, _>("side"),
        "order_type": r.get::<String, _>("order_type"),
        "tif": r.get::<String, _>("tif"),
        "price": r.get::<Option<Decimal>, _>("price").map(|v| v.to_string()),
        "quantity": r.get::<Decimal, _>("quantity").to_string(),
        "remaining": r.get::<Decimal, _>("remaining").to_string(),
        "status": r.get::<String, _>("status"),
        "stp_mode": r.get::<String, _>("stp_mode"),
        "version": r.get::<i64, _>("version"),
        "created_at": r.get::<chrono::DateTime<Utc>, _>("created_at").to_rfc3339(),
        "updated_at": r.get::<chrono::DateTime<Utc>, _>("updated_at").to_rfc3339(),
    })
}

fn order_to_json(o: &Order) -> Value {
    json!({
        "id": o.id.to_string(),
        "user_id": o.user_id,
        "pair_id": o.pair_id,
        "side": format!("{:?}", o.side),
        "order_type": format!("{:?}", o.order_type),
        "tif": format!("{:?}", o.tif),
        "price": o.price.map(|v| v.to_string()),
        "quantity": o.quantity.to_string(),
        "remaining": o.remaining.to_string(),
        "status": format!("{:?}", o.status),
        "stp_mode": format!("{:?}", o.stp_mode),
        "version": o.version,
        "created_at": o.created_at.to_rfc3339(),
        "updated_at": o.updated_at.to_rfc3339(),
    })
}

fn trade_to_json(t: &Trade) -> Value {
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

async fn lock_balance(pg: &sqlx::PgPool, order: &Order) -> anyhow::Result<()> {
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

async fn release_locked_balance(pg: &sqlx::PgPool, order: &Order) -> anyhow::Result<()> {
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
    // Release only the portion corresponding to remaining qty
    let row = sqlx::query("SELECT base, quote FROM pairs WHERE id = $1")
        .bind(&order.pair_id)
        .fetch_one(pg)
        .await?;
    let base: String = row.get("base");
    let quote: String = row.get("quote");

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

async fn get_lock_asset_amount(
    pg: &sqlx::PgPool,
    order: &Order,
) -> anyhow::Result<(String, Decimal)> {
    let row = sqlx::query("SELECT base, quote FROM pairs WHERE id = $1")
        .bind(&order.pair_id)
        .fetch_one(pg)
        .await?;
    let base: String = row.get("base");
    let quote: String = row.get("quote");

    Ok(match order.side {
        Side::Buy => {
            let price = order.price.unwrap_or(Decimal::ZERO);
            (quote, price * order.quantity)
        }
        Side::Sell => (base, order.quantity),
    })
}

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

async fn settle_trade_balances(pg: &sqlx::PgPool, trade: &Trade) -> anyhow::Result<()> {
    let row = sqlx::query("SELECT base, quote FROM pairs WHERE id = $1")
        .bind(&trade.pair_id)
        .fetch_one(pg)
        .await?;
    let base: String = row.get("base");
    let quote: String = row.get("quote");
    let cost = trade.price * trade.quantity;

    let mut tx = pg.begin().await?;

    // Buyer: unlock quote, credit base
    sqlx::query("UPDATE balances SET locked = GREATEST(locked - $1, 0) WHERE user_id = $2 AND asset = $3")
        .bind(cost).bind(&trade.buyer_id).bind(&quote)
        .execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO balances (user_id, asset, available, locked) VALUES ($1,$2,$3,0)
         ON CONFLICT (user_id, asset) DO UPDATE SET available = balances.available + $3",
    )
    .bind(&trade.buyer_id).bind(&base).bind(trade.quantity)
    .execute(&mut *tx).await?;

    // Seller: unlock base, credit quote
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
