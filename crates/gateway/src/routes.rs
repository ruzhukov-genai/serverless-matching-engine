//! Gateway route handlers — stateless REST API and WebSocket feeds.
//! All data is read from Dragonfly cache keys (pre-computed by the worker).
//! POST /api/orders validates and queues orders; returns 202 Accepted.

use axum::{
    extract::{Path, Query, State, WebSocketUpgrade, ws::Message, ws::WebSocket},
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use chrono::Utc;
use deadpool_redis::redis::AsyncCommands;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;
use uuid::Uuid;

use sme_shared::{OrderType, Side, SelfTradePreventionMode, TimeInForce};

use crate::AppState;

// ── Per-user TTL cache ───────────────────────────────────────────────────────
// Avoids hitting Dragonfly on every /api/orders and /api/portfolio request.
// Short TTL (2s) — eventual consistency acceptable for read-your-writes.

const USER_CACHE_TTL_MS: u128 = 2_000;

#[derive(Clone)]
pub struct UserCache {
    // Uses std::sync::RwLock (not tokio) — critical section is a HashMap lookup,
    // never holds across .await. std::sync avoids async scheduler overhead.
    inner: Arc<RwLock<HashMap<String, (std::time::Instant, String)>>>,
}

impl UserCache {
    pub fn new() -> Self {
        Self { inner: Arc::new(RwLock::new(HashMap::new())) }
    }

    /// Get cached value if TTL hasn't expired.
    pub async fn get(&self, key: &str) -> Option<String> {
        let map = self.inner.read().unwrap();
        if let Some((ts, val)) = map.get(key) {
            if ts.elapsed().as_millis() < USER_CACHE_TTL_MS {
                return Some(val.clone());
            }
        }
        None
    }

    /// Store a value with current timestamp.
    pub async fn set(&self, key: String, val: String) {
        let mut map = self.inner.write().unwrap();
        map.insert(key, (std::time::Instant::now(), val));
        // Evict stale entries if map grows large (>1000 entries)
        if map.len() > 1000 {
            map.retain(|_, (ts, _)| ts.elapsed().as_millis() < USER_CACHE_TTL_MS * 2);
        }
    }
}

// ── Error helper ─────────────────────────────────────────────────────────────

pub enum AppErrorKind {
    BadRequest,
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
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let status = match self.kind {
            AppErrorKind::BadRequest => StatusCode::BAD_REQUEST,
            AppErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        tracing::error!("gateway error: {:?}", self.inner);
        (status, Json(json!({"error": self.inner.to_string()}))).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError { kind: AppErrorKind::Internal, inner: e.into() }
    }
}

type HandlerResult<T> = Result<T, AppError>;

/// Return a pre-serialized JSON string as a response without parse→reserialize overhead.
/// Skips serde_json::from_str + Json() which was the dominant per-request cost.
#[inline]
fn raw_json(body: impl Into<String>) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body.into(),
    ).into_response()
}

/// Return a pre-serialized Arc<str> JSON as response — zero-copy from cache.
#[inline]
fn raw_json_arc(body: &std::sync::Arc<str>) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    ).into_response()
}

// ── GET /api/pairs ────────────────────────────────────────────────────────────

pub async fn list_pairs(State(s): State<AppState>) -> HandlerResult<Response> {
    if let Some(cached) = s.cache.get_latest("cache:pairs") {
        return Ok(raw_json_arc(&cached));
    }
    Ok(raw_json(r#"{"pairs":[]}"#))
}

// ── GET /api/orderbook/{pair_id} ──────────────────────────────────────────────

pub async fn get_orderbook(
    Path(pair_id): Path<String>,
    State(s): State<AppState>,
) -> HandlerResult<Response> {
    let cache_key = format!("cache:orderbook:{}", pair_id);
    if let Some(cached) = s.cache.get_latest(&cache_key) {
        return Ok(raw_json_arc(&cached));
    }
    Ok(raw_json(format!(r#"{{"pair":"{}","bids":[],"asks":[]}}"#, pair_id)))
}

// ── GET /api/trades/{pair_id} ─────────────────────────────────────────────────

pub async fn get_trades(
    Path(pair_id): Path<String>,
    State(s): State<AppState>,
) -> HandlerResult<Response> {
    let cache_key = format!("cache:trades:{}", pair_id);
    if let Some(cached) = s.cache.get_latest(&cache_key) {
        return Ok(raw_json_arc(&cached));
    }
    Ok(raw_json(format!(r#"{{"pair":"{}","trades":[]}}"#, pair_id)))
}

// ── GET /api/ticker/{pair_id} ─────────────────────────────────────────────────

pub async fn get_ticker(
    Path(pair_id): Path<String>,
    State(s): State<AppState>,
) -> HandlerResult<Response> {
    let cache_key = format!("cache:ticker:{}", pair_id);
    if let Some(cached) = s.cache.get_latest(&cache_key) {
        return Ok(raw_json_arc(&cached));
    }
    Ok(raw_json(format!(
        r#"{{"pair":"{}","last":null,"high_24h":null,"low_24h":null,"volume_24h":null}}"#,
        pair_id
    )))
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
    pub client_order_id: Option<String>,
}

pub async fn create_order(
    State(s): State<AppState>,
    Json(req): Json<CreateOrderRequest>,
) -> HandlerResult<impl IntoResponse> {
    let user_id = req.user_id.unwrap_or_else(|| "user-1".to_string());
    let tif = req.tif.unwrap_or(TimeInForce::GTC);
    let stp_mode = req.stp_mode.unwrap_or(SelfTradePreventionMode::None);

    if req.quantity <= Decimal::ZERO {
        return Err(AppError::bad_request(anyhow::anyhow!("quantity must be positive")));
    }
    if req.order_type == OrderType::Limit && req.price.is_none() {
        return Err(AppError::bad_request(anyhow::anyhow!("limit orders require price")));
    }
    if req.order_type == OrderType::Market && req.price.is_some() {
        return Err(AppError::bad_request(anyhow::anyhow!("market orders cannot have price")));
    }

    let now = Utc::now();
    let order_id = Uuid::new_v4();

    let order_json = json!({
        "id": order_id.to_string(),
        "user_id": user_id,
        "pair_id": req.pair_id,
        "side": format!("{:?}", req.side),
        "order_type": format!("{:?}", req.order_type),
        "tif": format!("{:?}", tif),
        "price": req.price.map(|v| v.to_string()),
        "quantity": req.quantity.to_string(),
        "stp_mode": format!("{:?}", stp_mode),
        "client_order_id": req.client_order_id,
        "created_at": now.to_rfc3339(),
    });

    // Queue the order for the worker (write path — direct Dragonfly)
    let mut conn = s.dragonfly.get().await?;
    let order_str = serde_json::to_string(&order_json)?;
    conn.lpush::<_, _, ()>(format!("queue:orders:{}", req.pair_id), &order_str).await?;

    tracing::info!(
        order_id = %order_id,
        pair_id = %req.pair_id,
        user_id = %user_id,
        "order queued for processing"
    );

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "order": {
                "id": order_id.to_string(),
                "status": "Pending",
                "user_id": user_id,
                "pair_id": req.pair_id,
                "side": format!("{:?}", req.side),
                "order_type": format!("{:?}", req.order_type),
                "tif": format!("{:?}", tif),
                "price": req.price.map(|v| v.to_string()),
                "quantity": req.quantity.to_string(),
                "remaining": req.quantity.to_string(),
                "status": "Queued",
                "stp_mode": format!("{:?}", stp_mode),
                "client_order_id": req.client_order_id,
                "created_at": now.to_rfc3339(),
            },
            "message": "Order queued for processing",
        })),
    ))
}

// ── GET /api/orders ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct OrdersQuery {
    pub user_id: Option<String>,
    pub pair_id: Option<String>,
    #[allow(dead_code)]
    pub limit: Option<i64>,
    #[allow(dead_code)]
    pub offset: Option<i64>,
}

pub async fn list_orders(
    Query(q): Query<OrdersQuery>,
    State(s): State<AppState>,
) -> HandlerResult<Response> {
    let user_id = q.user_id.unwrap_or_else(|| "user-1".to_string());
    let cache_key = format!("cache:orders:{}", user_id);

    // Check gateway-side TTL cache first (avoids Dragonfly round-trip)
    if let Some(cached) = s.user_cache.get(&cache_key).await {
        return Ok(raw_json(cached));
    }

    // Fallback to Dragonfly
    let mut conn = s.dragonfly.get().await?;
    let cached: String = conn.get(&cache_key).await.unwrap_or_else(|_| "{}".to_string());

    if cached != "{}" {
        s.user_cache.set(cache_key, cached.clone()).await;
        return Ok(raw_json(cached));
    }

    Ok(raw_json(r#"{"orders":[],"total":0,"limit":50,"offset":0}"#))
}

// ── DELETE /api/orders/{order_id} ─────────────────────────────────────────────

pub async fn cancel_order(
    Path(order_id): Path<Uuid>,
    State(s): State<AppState>,
) -> HandlerResult<impl IntoResponse> {
    let mut conn = s.dragonfly.get().await?;
    let cancel_json = json!({
        "type": "cancel",
        "order_id": order_id.to_string(),
        "requested_at": Utc::now().to_rfc3339(),
    });
    let cancel_str = serde_json::to_string(&cancel_json)?;
    conn.lpush::<_, _, ()>("queue:cancellations", &cancel_str).await?;

    Ok(Json(json!({
        "status": "cancel_queued",
        "order_id": order_id.to_string(),
        "message": "Cancellation request queued"
    })))
}

// ── PUT /api/orders/{order_id} ────────────────────────────────────────────────

pub async fn modify_order(
    Path(order_id): Path<Uuid>,
    State(s): State<AppState>,
    Json(_req): Json<CreateOrderRequest>,
) -> HandlerResult<impl IntoResponse> {
    let mut conn = s.dragonfly.get().await?;
    let modify_json = json!({
        "type": "modify",
        "order_id": order_id.to_string(),
        "requested_at": Utc::now().to_rfc3339(),
    });
    let modify_str = serde_json::to_string(&modify_json)?;
    conn.lpush::<_, _, ()>("queue:cancellations", &modify_str).await?;

    Ok(Json(json!({
        "status": "modify_queued",
        "cancelled_order_id": order_id.to_string(),
        "message": "Modification queued. Old order will be cancelled. Submit a new POST /api/orders to replace it."
    })))
}

// ── DELETE /api/orders (cancel all) ───────────────────────────────────────────

pub async fn cancel_all_orders(
    Query(q): Query<OrdersQuery>,
    State(s): State<AppState>,
) -> HandlerResult<impl IntoResponse> {
    let user_id = q.user_id.unwrap_or_else(|| "user-1".to_string());

    let mut conn = s.dragonfly.get().await?;
    let cancel_all_json = json!({
        "type": "cancel_all",
        "user_id": user_id,
        "pair_id": q.pair_id,
        "requested_at": Utc::now().to_rfc3339(),
    });
    let cancel_str = serde_json::to_string(&cancel_all_json)?;
    conn.lpush::<_, _, ()>("queue:cancellations", &cancel_str).await?;

    Ok(Json(json!({
        "status": "cancel_all_queued",
        "user_id": user_id,
        "message": "Cancel all request queued"
    })))
}

// ── GET /api/portfolio ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PortfolioQuery {
    pub user_id: Option<String>,
}

pub async fn get_portfolio(
    Query(q): Query<PortfolioQuery>,
    State(s): State<AppState>,
) -> HandlerResult<Response> {
    let user_id = q.user_id.unwrap_or_else(|| "user-1".to_string());
    let cache_key = format!("cache:portfolio:{}", user_id);

    // Check gateway-side TTL cache first
    if let Some(cached) = s.user_cache.get(&cache_key).await {
        return Ok(raw_json(cached));
    }

    // Fallback to Dragonfly
    let mut conn = s.dragonfly.get().await?;
    let cached: String = conn.get(&cache_key).await.unwrap_or_else(|_| "{}".to_string());

    if cached != "{}" {
        s.user_cache.set(cache_key, cached.clone()).await;
        return Ok(raw_json(cached));
    }

    Ok(raw_json(r#"{"balances":[]}"#))
}

// ── Dashboard API — read from CacheBroadcasts (zero Dragonfly) ────────────────

pub async fn get_metrics(State(s): State<AppState>) -> HandlerResult<Response> {
    Ok(raw_json(s.cache.get_latest("cache:metrics")
        .map(|v| v.to_string()).unwrap_or_else(|| "{}".to_string())))
}

pub async fn get_lock_metrics(State(s): State<AppState>) -> HandlerResult<Response> {
    Ok(raw_json(s.cache.get_latest("cache:lock_metrics")
        .map(|v| v.to_string()).unwrap_or_else(|| "{}".to_string())))
}

pub async fn get_throughput(State(s): State<AppState>) -> HandlerResult<Response> {
    Ok(raw_json(s.cache.get_latest("cache:throughput")
        .map(|v| v.to_string()).unwrap_or_else(|| r#"{"series":[]}"#.to_string())))
}

pub async fn get_latency_percentiles(State(s): State<AppState>) -> HandlerResult<Response> {
    Ok(raw_json(s.cache.get_latest("cache:latency_metrics")
        .map(|v| v.to_string()).unwrap_or_else(|| "{}".to_string())))
}

pub async fn get_audit(State(s): State<AppState>) -> HandlerResult<Response> {
    Ok(raw_json(s.cache.get_latest("cache:audit")
        .map(|v| v.to_string()).unwrap_or_else(|| r#"{"audit":[],"events":[]}"#.to_string())))
}

// ── WebSocket Feeds ───────────────────────────────────────────────────────────

/// Shared-broadcast WS handler — subscribes to a CacheBroadcasts channel
/// instead of polling Dragonfly directly. One Dragonfly poller feeds N clients.
async fn cache_broadcast_ws(
    cache_key: String,
    state: AppState,
    mut socket: WebSocket,
) {
    let mut rx = match state.cache.subscribe(&cache_key) {
        Some(rx) => rx,
        None => return, // unknown key — no poller registered
    };

    // Send the current value immediately so the client doesn't wait up to interval_ms
    if let Some(val) = state.cache.get_latest(&cache_key) {
        if socket.send(Message::Text(val.to_string().into())).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(val) => {
                        if socket.send(Message::Text(val.to_string().into())).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(Message::Ping(data))) => {
                        let _ = socket.send(Message::Pong(data)).await;
                    }
                    _ => {} // ignore text/binary from client
                }
            }
        }
    }
}

pub async fn ws_orderbook(
    Path(pair_id): Path<String>,
    State(s): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| {
        let cache_key = format!("cache:orderbook:{}", pair_id);
        cache_broadcast_ws(cache_key, s, socket)
    })
}

pub async fn ws_trades(
    Path(pair_id): Path<String>,
    State(s): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| {
        let cache_key = format!("cache:trades:{}", pair_id);
        cache_broadcast_ws(cache_key, s, socket)
    })
}

pub async fn ws_orders(
    Path(user_id): Path<String>,
    State(s): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_orders_ws(user_id, s, socket))
}

async fn handle_orders_ws(user_id: String, state: AppState, mut socket: WebSocket) {
    let mut rx = state.order_events_tx.subscribe();

    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(msg) => {
                        let is_mine = serde_json::from_str::<Value>(&msg)
                            .ok()
                            .and_then(|p| Some(p.get("user_id")?.as_str()? == user_id))
                            .unwrap_or(false);
                        if is_mine && socket.send(Message::Text(msg.into())).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(Message::Ping(data))) => {
                        let _ = socket.send(Message::Pong(data)).await;
                    }
                    _ => {}
                }
            }
        }
    }
}
