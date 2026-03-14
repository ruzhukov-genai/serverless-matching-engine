//! API route handlers — real implementations backed by Dragonfly + PostgreSQL.

use std::collections::HashMap;

use tokio::sync::mpsc;

use axum::{
    extract::{Path, Query, State, WebSocketUpgrade, ws::Message, ws::WebSocket},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use chrono::Utc;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use sme_shared::{
    cache, engine::match_order, lock, metrics, Order, OrderStatus, OrderType, Side,
    SelfTradePreventionMode, TimeInForce, Trade,
};

use crate::{AppState, PairConfig};

// ── Error helper ─────────────────────────────────────────────────────────────

pub enum AppErrorKind {
    BadRequest,
    NotFound,
    Conflict,
    Internal,
}

pub struct AppError {
    kind: AppErrorKind,
    inner: anyhow::Error,
}

impl AppError {
    pub fn bad_request(e: impl Into<anyhow::Error>) -> Self {
        AppError { kind: AppErrorKind::BadRequest, inner: e.into() }
    }
    pub fn not_found(e: impl Into<anyhow::Error>) -> Self {
        AppError { kind: AppErrorKind::NotFound, inner: e.into() }
    }
    pub fn conflict(e: impl Into<anyhow::Error>) -> Self {
        AppError { kind: AppErrorKind::Conflict, inner: e.into() }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let status = match self.kind {
            AppErrorKind::BadRequest => StatusCode::BAD_REQUEST,
            AppErrorKind::NotFound => StatusCode::NOT_FOUND,
            AppErrorKind::Conflict => StatusCode::CONFLICT,
            AppErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        tracing::error!("handler error: {:?}", self.inner);
        (status, Json(json!({"error": self.inner.to_string()}))).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError { kind: AppErrorKind::Internal, inner: e.into() }
    }
}

type HandlerResult<T> = Result<T, AppError>;

const PAIRS: &[&str] = &["BTC-USDT", "ETH-USDT", "SOL-USDT"];

// ── Async persistence ─────────────────────────────────────────────────────────

/// Job sent to the background persistence worker after a Lua match completes.
pub struct PersistJob {
    pub order: Order,
    pub trades: Vec<Trade>,
    pub lua_trades: Vec<cache::LuaTrade>,
}

/// Spawn a background worker that drains `rx` and persists each job to PostgreSQL.
pub fn spawn_persist_worker(
    pg: sqlx::PgPool,
    mut rx: mpsc::Receiver<PersistJob>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            if let Err(e) = process_persist_job(&pg, job).await {
                tracing::error!(error = %e, "async persist failed");
            }
        }
    })
}

/// Execute all DB writes for a single matched order (off the hot path).
async fn process_persist_job(pg: &sqlx::PgPool, job: PersistJob) -> anyhow::Result<()> {
    let persist_start = std::time::Instant::now();

    // 1. Audit: ORDER_CREATED
    insert_audit_event(
        pg,
        Some(&job.order.pair_id),
        "ORDER_CREATED",
        &json!({
            "order_id": job.order.id.to_string(),
            "user_id": job.order.user_id,
            "side": format!("{:?}", job.order.side),
            "order_type": format!("{:?}", job.order.order_type),
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
        "SELECT id, pair_id, buy_order_id, sell_order_id, buyer_id, seller_id, price, quantity, sequence, created_at
         FROM trades WHERE pair_id = $1 ORDER BY created_at DESC LIMIT 50",
    )
    .bind(&pair_id)
    .fetch_all(&s.pg)
    .await?;

    let trades: Vec<Value> = rows.iter().map(row_to_trade_json).collect();
    Ok(Json(json!({ "pair": pair_id, "trades": trades })))
}

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

fn row_to_trade_json(r: &sqlx::postgres::PgRow) -> Value {
    let trade = row_to_trade(r);
    trade_to_json(&trade)
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
    /// Client-provided idempotency key. If set, duplicate submissions
    /// with the same (user_id, client_order_id) return the existing order
    /// instead of creating a new one (exactly-once semantics).
    pub client_order_id: Option<String>,
}

pub async fn create_order(
    State(s): State<AppState>,
    Json(req): Json<CreateOrderRequest>,
) -> HandlerResult<impl IntoResponse> {
    let total_start = std::time::Instant::now();

    let user_id = req.user_id.unwrap_or_else(|| "user-1".to_string());
    let tif = req.tif.unwrap_or(TimeInForce::GTC);
    let stp_mode = req.stp_mode.unwrap_or(SelfTradePreventionMode::None);

    // ── IDEMPOTENCY CHECK ─────────────────────────────────────────────────
    // If client_order_id is provided, check for existing order with same
    // (user_id, client_order_id). Return cached result on duplicate.
    if let Some(ref coid) = req.client_order_id {
        let existing = sqlx::query(
            "SELECT id, status, remaining, price, quantity, side, order_type, pair_id
             FROM orders WHERE user_id = $1 AND client_order_id = $2"
        )
        .bind(&user_id)
        .bind(coid)
        .fetch_optional(&s.pg)
        .await?;

        if let Some(row) = existing {
            let order_id: Uuid = row.get("id");
            // Load trades for this order
            let trade_rows = sqlx::query(
                "SELECT id, pair_id, buy_order_id, sell_order_id, buyer_id, seller_id, price, quantity, created_at
                 FROM trades WHERE buy_order_id = $1 OR sell_order_id = $1"
            )
            .bind(order_id)
            .fetch_all(&s.pg)
            .await?;

            let trade_jsons: Vec<Value> = trade_rows.iter().map(|r| {
                json!({
                    "id": r.get::<Uuid, _>("id").to_string(),
                    "pair_id": r.get::<String, _>("pair_id"),
                    "buy_order_id": r.get::<Uuid, _>("buy_order_id").to_string(),
                    "sell_order_id": r.get::<Uuid, _>("sell_order_id").to_string(),
                    "buyer_id": r.get::<String, _>("buyer_id"),
                    "seller_id": r.get::<String, _>("seller_id"),
                    "price": r.get::<Decimal, _>("price").to_string(),
                    "quantity": r.get::<Decimal, _>("quantity").to_string(),
                })
            }).collect();

            let order_json = json!({
                "id": order_id.to_string(),
                "status": row.get::<String, _>("status"),
                "remaining": row.get::<Decimal, _>("remaining").to_string(),
                "price": row.get::<Option<Decimal>, _>("price").map(|v| v.to_string()),
                "quantity": row.get::<Decimal, _>("quantity").to_string(),
                "side": row.get::<String, _>("side"),
                "order_type": row.get::<String, _>("order_type"),
                "pair_id": row.get::<String, _>("pair_id"),
                "client_order_id": coid,
                "idempotent_replay": true,
            });

            tracing::info!(
                client_order_id = %coid,
                order_id = %order_id,
                "idempotent replay — returning cached result"
            );

            return Ok((
                StatusCode::OK, // 200, not 201 — this is a replay
                Json(json!({
                    "order": order_json,
                    "trades": trade_jsons,
                    "idempotent_replay": true,
                })),
            ));
        }
    }

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
        client_order_id: req.client_order_id.clone(),
    };

    // 1. Validate (outside lock) — uses in-memory cache, zero DB round-trips
    let pre_lock_start = std::time::Instant::now();
    validate_order_request(&s.pairs_cache, &order)
        .map_err(AppError::bad_request)?;

    // 2. Insert to DB (outside lock — unique constraint enforces idempotency)
    insert_order_db(&s.pg, &order).await?;

    // 3. Lock balance in DB (outside lock)
    lock_balance(&s.pg, &order).await
        .map_err(AppError::bad_request)?;
    let _pre_lock_ms = pre_lock_start.elapsed().as_millis() as u64;

    // ── ROUTE: FOK → OCC CAS path; all other orders → Lua atomic EVAL ───────
    //
    // FOK (Fill-or-Kill) requires knowing total available liquidity BEFORE
    // committing fills. A single-pass Lua script cannot do this atomically.
    // FOK orders use the OCC/CAS retry loop below (they are rare in practice).
    //
    // All other orders (GTC, IOC, Market) use `match_order_lua` — a single
    // Dragonfly EVAL call that loads the book, runs price-time matching, commits
    // all mutations, and returns trade results atomically with no lock or retry.

    let match_start = std::time::Instant::now();

    if tif == TimeInForce::FOK {
        // ── FOK: OCC/CAS retry loop (correctness over performance) ───────────
        const MAX_CAS_RETRIES: u32 = 10;
        let mut cas_retries = 0u32;
        let original_order = order.clone();
        let opposite_side = order.side.opposite();

        let result = loop {
            let (mut book, snapshot_version) = cache::load_order_book_snapshot(
                &s.dragonfly,
                &req.pair_id,
                opposite_side,
                order.price,
            )
            .await?;

            book.sort_by(|a, b| {
                let sa = sort_score(a);
                let sb = sort_score(b);
                sa.partial_cmp(&sb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.created_at.cmp(&b.created_at))
            });

            let r = match_order(&order, &mut book);
            order = r.incoming.clone();

            // FOK cancelled: exit without CAS write
            if r.trades.is_empty() && order.status == OrderStatus::Cancelled {
                cancel_order_in_db(&s.pg, order.id).await?;
                release_locked_balance(&s.pg, &order).await?;
                return Ok((
                    StatusCode::OK,
                    Json(json!({
                        "order": order_to_json(&order),
                        "trades": [],
                        "message": "FOK order cancelled — insufficient liquidity"
                    })),
                ));
            }

            let incoming_if_resting = if order.status == OrderStatus::New
                || order.status == OrderStatus::PartiallyFilled
            {
                Some(&order)
            } else {
                None
            };

            match cache::apply_book_mutations_cas(
                &s.dragonfly,
                &req.pair_id,
                snapshot_version,
                &r.book_updates,
                incoming_if_resting,
            )
            .await?
            {
                cache::CasResult::Ok => break r,
                cache::CasResult::Conflict => {
                    cas_retries += 1;
                    if cas_retries >= MAX_CAS_RETRIES {
                        return Err(AppError::conflict(anyhow::anyhow!(
                            "order book contention: too many retries"
                        )));
                    }
                    order = original_order.clone();
                    continue;
                }
            }
        };

        let lua_ms = match_start.elapsed().as_millis() as u64;
        let total_ms = total_start.elapsed().as_millis() as u64;
        let trade_count = result.trades.len();

        {
            let pool = s.dragonfly.clone();
            let pair_id = req.pair_id.clone();
            tokio::spawn(async move {
                let _ = metrics::record_match_latency(&pool, &pair_id, lua_ms).await;
                let _ = metrics::record_lock_wait(&pool, &pair_id, 0).await;
                let _ = metrics::increment_order_count(&pool, &pair_id).await;
                if trade_count > 0 {
                    let _ = metrics::increment_trade_count(&pool, &pair_id, trade_count as u64).await;
                }
            });
        }

        let post_lock_start = std::time::Instant::now();

        insert_audit_event(
            &s.pg,
            Some(&req.pair_id),
            "ORDER_CREATED",
            &json!({
                "order_id": order.id.to_string(),
                "user_id": order.user_id,
                "side": format!("{:?}", order.side),
                "order_type": format!("{:?}", order.order_type),
                "price": order.price.map(|v| v.to_string()),
                "quantity": order.quantity.to_string(),
            }),
        )
        .await
        .ok();

        let trade_jsons = persist_trades(&s.pg, &result.trades).await?;

        let mut all_updates: Vec<&Order> = result.book_updates.iter().collect();
        all_updates.push(&order);
        batch_update_orders_db(&s.pg, &all_updates).await?;

        for upd in &result.book_updates {
            let event_type = match upd.status {
                OrderStatus::Filled => "ORDER_FILLED",
                OrderStatus::PartiallyFilled => "ORDER_PARTIALLY_FILLED",
                OrderStatus::Cancelled => "ORDER_CANCELLED",
                _ => continue,
            };
            insert_audit_event(
                &s.pg,
                Some(&req.pair_id),
                event_type,
                &json!({ "order_id": upd.id.to_string(), "remaining": upd.remaining.to_string() }),
            )
            .await
            .ok();
        }

        if order.status == OrderStatus::Cancelled || order.status == OrderStatus::Filled {
            release_remaining_locked(&s.pg, &order).await?;
        }

        let post_lock_ms = post_lock_start.elapsed().as_millis() as u64;

        tracing::info!(
            lua_ms = lua_ms,
            match_ms = 0_u64,
            post_lock_ms = post_lock_ms,
            total_ms = total_ms,
            trade_count = trade_count,
            pair_id = %req.pair_id,
            fok_occ_path = true,
            "order complete"
        );

        return Ok((
            StatusCode::CREATED,
            Json(json!({
                "order": order_to_json(&order),
                "trades": trade_jsons,
            })),
        ));
    }

    // ── Non-FOK: single atomic Lua EVAL — no lock, no retry ──────────────────
    let lua_result = cache::match_order_lua(&s.dragonfly, &order).await?;
    let lua_ms = match_start.elapsed().as_millis() as u64;

    // Apply Lua result to the incoming order struct
    order.remaining = lua_result.remaining;
    order.status = lua_result.status;

    let trade_count = lua_result.trades.len();

    // Metrics (best-effort)
    {
        let pool = s.dragonfly.clone();
        let pair_id = req.pair_id.clone();
        tokio::spawn(async move {
            let _ = metrics::record_match_latency(&pool, &pair_id, lua_ms).await;
            let _ = metrics::record_lock_wait(&pool, &pair_id, 0).await;
            let _ = metrics::increment_order_count(&pool, &pair_id).await;
            if trade_count > 0 {
                let _ = metrics::increment_trade_count(&pool, &pair_id, trade_count as u64).await;
            }
        });
    }

    // ── Respond immediately; persist in background ────────────────────────
    let respond_start = std::time::Instant::now();

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
    match s.persist_tx.try_send(job) {
        Ok(()) => {}
        Err(e) => {
            // Channel full or closed — bypass channel and spawn directly
            tracing::warn!("persist channel full, spawning direct persist task");
            let job = e.into_inner();
            let pg = s.pg_bg.clone();
            tokio::spawn(async move {
                if let Err(err) = process_persist_job(&pg, job).await {
                    tracing::error!(error = %err, "direct persist task failed");
                }
            });
        }
    }

    let respond_ms = respond_start.elapsed().as_millis() as u64;
    let total_ms = total_start.elapsed().as_millis() as u64;

    tracing::info!(
        lua_ms = lua_ms,
        respond_ms = respond_ms,
        total_ms = total_ms,
        trade_count = trade_count,
        pair_id = %req.pair_id,
        "order complete"
    );

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
    let order = cancel_order_with_lock(&s.pg, &s.dragonfly, order_id, "cancel").await?;

    insert_audit_event(
        &s.pg,
        Some(&order.pair_id),
        "ORDER_CANCELLED",
        &json!({ "order_id": order_id.to_string(), "user_id": order.user_id }),
    )
    .await
    .ok();

    Ok(Json(json!({ "status": "cancelled", "order_id": order_id.to_string() })))
}

// ── PUT /api/orders/{order_id} ────────────────────────────────────────────────

pub async fn modify_order(
    Path(order_id): Path<Uuid>,
    State(s): State<AppState>,
    Json(_req): Json<CreateOrderRequest>,
) -> HandlerResult<impl IntoResponse> {
    let _old_order = cancel_order_with_lock(&s.pg, &s.dragonfly, order_id, "modify").await?;

    Ok(Json(json!({
        "cancelled_order_id": order_id.to_string(),
        "message": "Old order cancelled. Submit a new POST /api/orders to replace it.",
    })))
}

// ── GET /api/orders ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct OrdersQuery {
    pub user_id: Option<String>,
    pub pair_id: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn list_orders(
    Query(q): Query<OrdersQuery>,
    State(s): State<AppState>,
) -> HandlerResult<impl IntoResponse> {
    let user_id = q.user_id.unwrap_or_else(|| "user-1".to_string());
    let limit = q.limit.unwrap_or(50).min(500);
    let offset = q.offset.unwrap_or(0).max(0);

    let (count_row, rows) = if let Some(ref pair_id) = q.pair_id {
        let c = sqlx::query("SELECT COUNT(*) as cnt FROM orders WHERE user_id = $1 AND status IN ('New','PartiallyFilled') AND pair_id = $2")
            .bind(&user_id).bind(pair_id).fetch_one(&s.pg).await?;
        let r = sqlx::query(
            "SELECT id, user_id, pair_id, side, order_type, tif, price, quantity, remaining, status, stp_mode, version, sequence, created_at, updated_at, client_order_id
             FROM orders WHERE user_id = $1 AND status IN ('New','PartiallyFilled') AND pair_id = $2 ORDER BY created_at DESC LIMIT $3 OFFSET $4",
        ).bind(&user_id).bind(pair_id).bind(limit).bind(offset).fetch_all(&s.pg).await?;
        (c, r)
    } else {
        let c = sqlx::query("SELECT COUNT(*) as cnt FROM orders WHERE user_id = $1 AND status IN ('New','PartiallyFilled')")
            .bind(&user_id).fetch_one(&s.pg).await?;
        let r = sqlx::query(
            "SELECT id, user_id, pair_id, side, order_type, tif, price, quantity, remaining, status, stp_mode, version, sequence, created_at, updated_at, client_order_id
             FROM orders WHERE user_id = $1 AND status IN ('New','PartiallyFilled') ORDER BY created_at DESC LIMIT $2 OFFSET $3",
        ).bind(&user_id).bind(limit).bind(offset).fetch_all(&s.pg).await?;
        (c, r)
    };

    let total: i64 = count_row.get("cnt");
    let orders: Vec<Value> = rows.iter().map(row_to_order_json).collect();
    Ok(Json(json!({ "orders": orders, "total": total, "limit": limit, "offset": offset })))
}

// ── DELETE /api/orders (cancel all) ───────────────────────────────────────────

pub async fn cancel_all_orders(
    Query(q): Query<OrdersQuery>,
    State(s): State<AppState>,
) -> HandlerResult<impl IntoResponse> {
    let user_id = q.user_id.unwrap_or_else(|| "user-1".to_string());

    let result = if let Some(ref pair_id) = q.pair_id {
        sqlx::query("UPDATE orders SET status = 'Cancelled', updated_at = NOW() WHERE user_id = $1 AND status IN ('New','PartiallyFilled') AND pair_id = $2")
            .bind(&user_id).bind(pair_id).execute(&s.pg).await?
    } else {
        sqlx::query("UPDATE orders SET status = 'Cancelled', updated_at = NOW() WHERE user_id = $1 AND status IN ('New','PartiallyFilled')")
            .bind(&user_id).execute(&s.pg).await?
    };

    // Also release locked balances
    sqlx::query("UPDATE balances SET available = available + locked, locked = 0 WHERE user_id = $1 AND locked > 0")
        .bind(&user_id).execute(&s.pg).await?;

    Ok(Json(json!({ "cancelled": result.rows_affected() })))
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
    use tokio::time::{Duration, interval};
    let mut ticker = interval(Duration::from_millis(500));
    let mut last_version: i64 = -1;

    loop {
        ticker.tick().await;

        let version = cache::get_version(&state.dragonfly, &pair_id)
            .await
            .unwrap_or(0);

        if version == last_version {
            continue;
        }
        last_version = version;

        let bids = cache::load_order_book(&state.dragonfly, &pair_id, Side::Buy)
            .await
            .unwrap_or_default();
        let asks = cache::load_order_book(&state.dragonfly, &pair_id, Side::Sell)
            .await
            .unwrap_or_default();

        let msg = serde_json::to_string(&json!({
            "type": "snapshot",
            "pair": pair_id,
            "version": version,
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
    use tokio::time::{Duration, interval};
    let mut ticker = interval(Duration::from_millis(500));
    let mut last_seen: Option<chrono::DateTime<Utc>> = None;

    loop {
        ticker.tick().await;

        let query = if let Some(since) = last_seen {
            sqlx::query(
                "SELECT id, pair_id, buy_order_id, sell_order_id, buyer_id, seller_id, price, quantity, sequence, created_at
                 FROM trades WHERE pair_id = $1 AND created_at > $2 ORDER BY created_at DESC LIMIT 20",
            )
            .bind(&pair_id)
            .bind(since)
            .fetch_all(&state.pg)
            .await
        } else {
            sqlx::query(
                "SELECT id, pair_id, buy_order_id, sell_order_id, buyer_id, seller_id, price, quantity, sequence, created_at
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

/// GET /api/metrics — aggregate order/trade counts from Dragonfly metrics keys.
pub async fn get_metrics(State(s): State<AppState>) -> HandlerResult<impl IntoResponse> {
    let mut total_orders: i64 = 0;
    let mut total_trades: i64 = 0;

    for pair_id in PAIRS {
        total_orders += metrics::get_order_count(&s.dragonfly, pair_id).await.unwrap_or(0);
        total_trades += metrics::get_trade_count(&s.dragonfly, pair_id).await.unwrap_or(0);
    }

    // Also collect latency P50 across all pairs for the KPI "latency" field
    let mut all_latency: Vec<u64> = Vec::new();
    for pair_id in PAIRS {
        let samples = metrics::get_latency_samples(&s.dragonfly, pair_id, 100).await.unwrap_or_default();
        all_latency.extend(samples);
    }
    let (p50, p95, p99) = metrics::compute_percentiles(all_latency);

    Ok(Json(json!({
        "orders_per_sec": total_orders,
        "matches_per_sec": total_trades,
        "trades_per_sec": total_trades,
        "active_pairs": 3,
        "active_workers": 1,
        "latency": {
            "p50": p50,
            "p95": p95,
            "p99": p99,
        },
        "streams": {},
    })))
}

/// GET /api/metrics/locks — read from Dragonfly lock_wait lists.
pub async fn get_lock_metrics(State(s): State<AppState>) -> HandlerResult<impl IntoResponse> {
    let mut all_waits: Vec<u64> = Vec::new();
    for pair_id in PAIRS {
        let samples = metrics::get_lock_wait_samples(&s.dragonfly, pair_id, 1000).await.unwrap_or_default();
        all_waits.extend(samples);
    }

    let (avg_wait_ms, contention_rate) = if all_waits.is_empty() {
        (0.0f64, 0.0f64)
    } else {
        let avg = all_waits.iter().sum::<u64>() as f64 / all_waits.len() as f64;
        // contention = fraction of waits > 1ms
        let contended = all_waits.iter().filter(|&&v| v > 1).count();
        let rate = contended as f64 / all_waits.len() as f64;
        (avg, rate)
    };

    Ok(Json(json!({
        "contention_rate": contention_rate,
        "avg_wait_ms": avg_wait_ms,
        "retry_count": all_waits.len(),
        "failures": 0,
    })))
}

/// GET /api/metrics/throughput — trade counts per minute from DB.
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

/// GET /api/metrics/latency — P50/P95/P99 from Dragonfly latency samples.
pub async fn get_latency_percentiles(State(s): State<AppState>) -> HandlerResult<impl IntoResponse> {
    let mut all_samples: Vec<u64> = Vec::new();
    let mut per_pair: Vec<Value> = Vec::new();

    for pair_id in PAIRS {
        let samples = metrics::get_latency_samples(&s.dragonfly, pair_id, 1000).await.unwrap_or_default();
        let (p50, p95, p99) = metrics::compute_percentiles(samples.clone());
        per_pair.push(json!({
            "pair_id": pair_id,
            "sample_count": samples.len(),
            "p50": p50,
            "p95": p95,
            "p99": p99,
        }));
        all_samples.extend(samples);
    }

    let (p50, p95, p99) = metrics::compute_percentiles(all_samples.clone());

    Ok(Json(json!({
        "sample_count": all_samples.len(),
        "p50": p50,
        "p95": p95,
        "p99": p99,
        "per_pair": per_pair,
    })))
}

/// GET /api/audit — last 50 audit events from DB.
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

    // Support both /api/audit and /api/metrics/audit paths
    Ok(Json(json!({ "audit": entries, "events": entries })))
}

// ── Audit helpers ─────────────────────────────────────────────────────────────

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
fn validate_order_request(
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

async fn insert_order_db(pg: &sqlx::PgPool, order: &Order) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO orders (id, user_id, pair_id, side, order_type, tif, price, quantity, remaining, status, stp_mode, version, created_at, updated_at, client_order_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
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
    .bind(format!("{:?}", order.status))
    .bind(Utc::now())
    .bind(order.id)
    .execute(pg)
    .await?;
    Ok(())
}

/// Batch update multiple orders in a single transaction.
/// Replaces the per-order loop (N separate round-trips) with 1 transaction.
async fn batch_update_orders_db(pg: &sqlx::PgPool, orders: &[&Order]) -> anyhow::Result<()> {
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
        "SELECT id, user_id, pair_id, side, order_type, tif, price, quantity, remaining, status, stp_mode, version, sequence, created_at, updated_at, client_order_id
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
        sequence: r.get::<Option<i64>, _>("sequence").unwrap_or(0),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        client_order_id: r.get("client_order_id"),
    }
}

fn row_to_order_json(r: &sqlx::postgres::PgRow) -> Value {
    let order = row_to_order(r);
    order_to_json(&order)
}

fn order_to_json(o: &Order) -> Value {
    let mut j = json!({
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
    });
    if let Some(ref coid) = o.client_order_id {
        j["client_order_id"] = json!(coid);
    }
    j
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

async fn cancel_order_with_lock(
    pg: &sqlx::PgPool,
    dragonfly: &deadpool_redis::Pool,
    order_id: Uuid,
    operation_name: &str,
) -> HandlerResult<Order> {
    // Initial load without lock
    let order = load_order_db(pg, order_id).await?
        .ok_or_else(|| AppError::not_found(anyhow::anyhow!("order not found")))?;

    if order.status != OrderStatus::New && order.status != OrderStatus::PartiallyFilled {
        return Err(AppError::conflict(anyhow::anyhow!(
            "order not cancellable: status={:?}",
            order.status
        )));
    }

    // Acquire lock and double-check
    let worker_id = format!("{}-{order_id}", operation_name);
    let guard = lock::acquire_lock(dragonfly, &order.pair_id, &worker_id).await?;

    // Re-check status inside lock
    let order = load_order_db(pg, order_id).await?
        .ok_or_else(|| AppError::not_found(anyhow::anyhow!("order not found")))?;
    if order.status != OrderStatus::New && order.status != OrderStatus::PartiallyFilled {
        guard.release().await;
        return Err(AppError::conflict(anyhow::anyhow!(
            "order not cancellable: status={:?}",
            order.status
        )));
    }

    // Remove from book and release lock
    cache::remove_order_from_book(dragonfly, &order).await?;
    cache::increment_version(dragonfly, &order.pair_id).await?;
    guard.release().await;

    // Cancel in DB and release balance
    cancel_order_in_db(pg, order_id).await?;
    release_locked_balance(pg, &order).await?;

    Ok(order)
}

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

fn sort_score(order: &Order) -> f64 {
    order.book_score()
}
