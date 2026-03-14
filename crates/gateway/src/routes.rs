//! Gateway route handlers — stateless REST API and WebSocket feeds.
//! All data is read from Dragonfly cache keys (pre-computed by the worker).
//! POST /api/orders validates and queues orders; returns 202 Accepted.

use axum::{
    extract::{Path, Query, State, WebSocketUpgrade, ws::Message, ws::WebSocket},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use chrono::Utc;
use deadpool_redis::redis::AsyncCommands;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::broadcast;
use uuid::Uuid;

use sme_shared::{OrderStatus, OrderType, Side, SelfTradePreventionMode, TimeInForce};

use crate::AppState;

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

// ── GET /api/pairs ────────────────────────────────────────────────────────────

pub async fn list_pairs(State(s): State<AppState>) -> HandlerResult<impl IntoResponse> {
    let mut conn = s.dragonfly.get().await?;
    let cached: String = conn.get("cache:pairs").await.unwrap_or_else(|_| "{}".to_string());
    
    // Return the cached JSON directly (worker pre-computes this)
    if cached != "{}" {
        let parsed: Value = serde_json::from_str(&cached).unwrap_or(json!({"pairs": []}));
        return Ok(Json(parsed));
    }
    
    // Fallback if cache is empty
    Ok(Json(json!({"pairs": []})))
}

// ── GET /api/orderbook/{pair_id} ──────────────────────────────────────────────

pub async fn get_orderbook(
    Path(pair_id): Path<String>,
    State(s): State<AppState>,
) -> HandlerResult<impl IntoResponse> {
    let mut conn = s.dragonfly.get().await?;
    let cache_key = format!("cache:orderbook:{}", pair_id);
    let cached: String = conn.get(&cache_key).await.unwrap_or_else(|_| "{}".to_string());
    
    if cached != "{}" {
        let parsed: Value = serde_json::from_str(&cached).unwrap_or(json!({
            "pair": pair_id,
            "bids": [],
            "asks": []
        }));
        return Ok(Json(parsed));
    }
    
    // Fallback empty orderbook
    Ok(Json(json!({
        "pair": pair_id,
        "bids": [],
        "asks": []
    })))
}

// ── GET /api/trades/{pair_id} ─────────────────────────────────────────────────

pub async fn get_trades(
    Path(pair_id): Path<String>,
    State(s): State<AppState>,
) -> HandlerResult<impl IntoResponse> {
    let mut conn = s.dragonfly.get().await?;
    let cache_key = format!("cache:trades:{}", pair_id);
    let cached: String = conn.get(&cache_key).await.unwrap_or_else(|_| "{}".to_string());
    
    if cached != "{}" {
        let parsed: Value = serde_json::from_str(&cached).unwrap_or(json!({
            "pair": pair_id,
            "trades": []
        }));
        return Ok(Json(parsed));
    }
    
    // Fallback empty trades
    Ok(Json(json!({
        "pair": pair_id,
        "trades": []
    })))
}

// ── GET /api/ticker/{pair_id} ─────────────────────────────────────────────────

pub async fn get_ticker(
    Path(pair_id): Path<String>,
    State(s): State<AppState>,
) -> HandlerResult<impl IntoResponse> {
    let mut conn = s.dragonfly.get().await?;
    let cache_key = format!("cache:ticker:{}", pair_id);
    let cached: String = conn.get(&cache_key).await.unwrap_or_else(|_| "{}".to_string());
    
    if cached != "{}" {
        let parsed: Value = serde_json::from_str(&cached).unwrap_or(json!({
            "pair": pair_id,
            "last": null,
            "high_24h": null,
            "low_24h": null,
            "volume_24h": null
        }));
        return Ok(Json(parsed));
    }
    
    // Fallback empty ticker
    Ok(Json(json!({
        "pair": pair_id,
        "last": null,
        "high_24h": null,
        "low_24h": null,
        "volume_24h": null
    })))
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

    // Basic validation (could load pairs from cache for more thorough validation)
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

    // Create order JSON for the queue
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

    // Queue the order for the worker
    let mut conn = s.dragonfly.get().await?;
    let order_str = serde_json::to_string(&order_json)?;
    conn.lpush::<_, _, ()>("queue:orders", &order_str).await?;

    tracing::info!(
        order_id = %order_id,
        pair_id = %req.pair_id,
        user_id = %user_id,
        "order queued for processing"
    );

    // Return 202 Accepted with the order details
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "order": {
                "id": order_id.to_string(),
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
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn list_orders(
    Query(q): Query<OrdersQuery>,
    State(s): State<AppState>,
) -> HandlerResult<impl IntoResponse> {
    let user_id = q.user_id.unwrap_or_else(|| "user-1".to_string());
    
    // Get cached user orders (worker maintains this cache)
    let mut conn = s.dragonfly.get().await?;
    let cache_key = format!("cache:orders:{}", user_id);
    let cached: String = conn.get(&cache_key).await.unwrap_or_else(|_| "{}".to_string());
    
    if cached != "{}" {
        let parsed: Value = serde_json::from_str(&cached).unwrap_or(json!({
            "orders": [],
            "total": 0,
            "limit": 50,
            "offset": 0
        }));
        return Ok(Json(parsed));
    }
    
    // Fallback empty response
    Ok(Json(json!({
        "orders": [],
        "total": 0,
        "limit": q.limit.unwrap_or(50),
        "offset": q.offset.unwrap_or(0)
    })))
}

// ── DELETE /api/orders/{order_id} ─────────────────────────────────────────────

pub async fn cancel_order(
    Path(order_id): Path<Uuid>,
    State(s): State<AppState>,
) -> HandlerResult<impl IntoResponse> {
    // Queue the cancellation request
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
    // Queue the modification (which is cancel + new order)
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

    // Queue the cancel-all request
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
) -> HandlerResult<impl IntoResponse> {
    let user_id = q.user_id.unwrap_or_else(|| "user-1".to_string());

    let mut conn = s.dragonfly.get().await?;
    let cache_key = format!("cache:portfolio:{}", user_id);
    let cached: String = conn.get(&cache_key).await.unwrap_or_else(|_| "{}".to_string());
    
    if cached != "{}" {
        let parsed: Value = serde_json::from_str(&cached).unwrap_or(json!({"balances": []}));
        return Ok(Json(parsed));
    }
    
    // Fallback empty balances
    Ok(Json(json!({"balances": []})))
}

// ── Dashboard API ─────────────────────────────────────────────────────────────

pub async fn get_metrics(State(s): State<AppState>) -> HandlerResult<impl IntoResponse> {
    let mut conn = s.dragonfly.get().await?;
    let cached: String = conn.get("cache:metrics").await.unwrap_or_else(|_| "{}".to_string());
    
    let parsed: Value = serde_json::from_str(&cached).unwrap_or(json!({}));
    Ok(Json(parsed))
}

pub async fn get_lock_metrics(State(s): State<AppState>) -> HandlerResult<impl IntoResponse> {
    let mut conn = s.dragonfly.get().await?;
    let cached: String = conn.get("cache:lock_metrics").await.unwrap_or_else(|_| "{}".to_string());
    
    let parsed: Value = serde_json::from_str(&cached).unwrap_or(json!({}));
    Ok(Json(parsed))
}

pub async fn get_throughput(State(s): State<AppState>) -> HandlerResult<impl IntoResponse> {
    let mut conn = s.dragonfly.get().await?;
    let cached: String = conn.get("cache:throughput").await.unwrap_or_else(|_| "{}".to_string());
    
    let parsed: Value = serde_json::from_str(&cached).unwrap_or(json!({"series": []}));
    Ok(Json(parsed))
}

pub async fn get_latency_percentiles(State(s): State<AppState>) -> HandlerResult<impl IntoResponse> {
    let mut conn = s.dragonfly.get().await?;
    let cached: String = conn.get("cache:latency_metrics").await.unwrap_or_else(|_| "{}".to_string());
    
    let parsed: Value = serde_json::from_str(&cached).unwrap_or(json!({}));
    Ok(Json(parsed))
}

pub async fn get_audit(State(s): State<AppState>) -> HandlerResult<impl IntoResponse> {
    let mut conn = s.dragonfly.get().await?;
    let cached: String = conn.get("cache:audit").await.unwrap_or_else(|_| "{}".to_string());
    
    let parsed: Value = serde_json::from_str(&cached).unwrap_or(json!({"audit": [], "events": []}));
    Ok(Json(parsed))
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
    let cache_key = format!("cache:orderbook:{}", pair_id);
    tracing::debug!(pair_id = %pair_id, "orderbook WS connected");

    loop {
        ticker.tick().await;
        tracing::debug!("orderbook WS tick");

        let cached: String = match state.dragonfly.get().await {
            Ok(mut conn) => {
                tracing::debug!("got DF conn");
                match conn.get(&cache_key).await {
                    Ok(v) => v,
                    Err(e) => { tracing::warn!(error=%e, "GET failed"); continue; }
                }
            }
            Err(e) => { tracing::warn!(error=%e, "pool get failed"); continue; }
        };

        tracing::debug!(len = cached.len(), "sending");
        if !cached.is_empty() && cached != "{}" {
            let msg = Message::Text(cached.into());
            match socket.send(msg).await {
                Ok(()) => tracing::debug!("sent ok"),
                Err(e) => { tracing::warn!(error=%e, "send failed"); break; }
            }
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
    let cache_key = format!("cache:trades:{}", pair_id);

    loop {
        ticker.tick().await;

        let cached: String = match state.dragonfly.get().await {
            Ok(mut conn) => match conn.get(&cache_key).await {
                Ok(v) => v,
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        if !cached.is_empty() && cached != "{}" {
            if socket.send(Message::Text(cached.into())).await.is_err() {
                break;
            }
        }
    }
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

    // Stream real-time updates — filter by user_id
    loop {
        match rx.recv().await {
            Ok(msg) => {
                // Each message is JSON with a "user_id" field — only forward matching ones
                let is_mine = serde_json::from_str::<Value>(&msg)
                    .ok()
                    .and_then(|p| Some(p.get("user_id")?.as_str()? == user_id))
                    .unwrap_or(false);
                if is_mine && socket.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::debug!(user = %user_id, lagged = n, "ws orders client lagged");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}