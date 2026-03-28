//! Gateway route handlers — stateless REST API and WebSocket feeds.
//! All data is read from Valkey cache keys (pre-computed by the worker).
//! POST /api/orders validates and queues orders; returns 202 Accepted.

use axum::{
    extract::{Path, Query, State, WebSocketUpgrade, ws::Message, ws::WebSocket},
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response, Sse, sse::Event},
};
// futures_util used by async_stream internally
use chrono::Utc;
use deadpool_redis::redis::AsyncCommands;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;
use uuid::Uuid;

use sme_shared::{OrderStatus, OrderType, Side, SelfTradePreventionMode, TimeInForce};

// ── Static string helpers (no format!("{:?}") allocations) ───────────────────

fn side_str(s: Side) -> &'static str {
    match s { Side::Buy => "Buy", Side::Sell => "Sell" }
}

fn order_type_str(ot: OrderType) -> &'static str {
    match ot { OrderType::Limit => "Limit", OrderType::Market => "Market" }
}

fn tif_str(tif: TimeInForce) -> &'static str {
    match tif { TimeInForce::GTC => "GTC", TimeInForce::IOC => "IOC", TimeInForce::FOK => "FOK" }
}

#[allow(dead_code)]
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
        SelfTradePreventionMode::None        => "None",
        SelfTradePreventionMode::CancelMaker => "CancelMaker",
        SelfTradePreventionMode::CancelTaker => "CancelTaker",
        SelfTradePreventionMode::CancelBoth  => "CancelBoth",
    }
}

use crate::AppState;

// ── Per-user TTL cache ───────────────────────────────────────────────────────
// Avoids hitting Valkey on every /api/orders and /api/portfolio request.
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

/// Return a pre-serialized Arc<str> JSON as response — uses Bytes for zero-copy.
/// Arc<str> → Bytes avoids the String allocation that .to_string() would create.
#[inline]
fn raw_json_arc(body: &std::sync::Arc<str>) -> Response {
    // Convert Arc<str> to Bytes without allocating a new String.
    // This is the hot path for all cached responses.
    let bytes = bytes::Bytes::copy_from_slice(body.as_bytes());
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        bytes,
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

#[derive(Deserialize)]
pub struct OrderbookQuery {
    pub depth: Option<i32>,
}

/// Truncate an orderbook JSON to at most `depth` bids + asks.
/// Returns the original string if depth is None, out of range, or parsing fails.
fn truncate_orderbook_depth(ob_json: Option<&str>, depth: Option<i32>) -> String {
    let raw = ob_json.unwrap_or(r#"{"bids":[],"asks":[]}"#);
    let depth = match depth {
        Some(d) if d > 0 && d < 500 => d as usize,
        _ => return raw.to_string(),
    };
    let Ok(mut data) = serde_json::from_str::<serde_json::Value>(raw) else {
        return raw.to_string();
    };
    if let Some(obj) = data.as_object_mut() {
        if let Some(bids) = obj.get_mut("bids").and_then(|v| v.as_array_mut()) {
            bids.truncate(depth);
        }
        if let Some(asks) = obj.get_mut("asks").and_then(|v| v.as_array_mut()) {
            asks.truncate(depth);
        }
    }
    serde_json::to_string(&data).unwrap_or_else(|_| raw.to_string())
}

pub async fn get_orderbook(
    Path(pair_id): Path<String>,
    Query(q): Query<OrderbookQuery>,
    State(s): State<AppState>,
) -> HandlerResult<Response> {
    let cache_key = format!("cache:orderbook:{}", pair_id);

    // Read directly from Valkey — authoritative, consistent across all Lambda instances.
    // (In-memory cache is only used for WS fan-out; REST reads from Valkey directly.)
    if let Ok(mut conn) = s.redis.get().await {
        use deadpool_redis::redis::AsyncCommands;
        if let Ok(Some(val)) = conn.get::<_, Option<String>>(&cache_key).await {
            if !val.is_empty() {
                return Ok(raw_json(truncate_orderbook_depth(Some(&val), q.depth)));
            }
        }
    }

    // Fallback: in-memory cache (e.g. if Valkey is unreachable)
    if let Some(cached) = s.cache.get_latest(&cache_key) {
        if q.depth.is_some() {
            return Ok(raw_json(truncate_orderbook_depth(Some(&cached), q.depth)));
        }
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
        "side": side_str(req.side),
        "order_type": order_type_str(req.order_type),
        "tif": tif_str(tif),
        "price": req.price.map(|v| v.to_string()),
        "quantity": req.quantity.to_string(),
        "stp_mode": stp_str(stp_mode),
        "client_order_id": req.client_order_id,
        "created_at": now.to_rfc3339(),
        "received_at": now.to_rfc3339(),
    });

    let order_str = serde_json::to_string(&order_json)?;

    // Dispatch order to worker — mode selected by ORDER_DISPATCH_MODE env var
    match s.dispatch_mode {
        crate::DispatchMode::Inline => {
            // Process order inline — no cross-Lambda invoke.
            // Matching runs in-process: eliminates ~800ms Lambda-to-Lambda overhead.
            if let Err(e) = crate::worker::process_order_inline(&order_json).await {
                tracing::error!(order_id = %order_id, error = %e, "inline order processing failed");
            }
        }
        crate::DispatchMode::Queue => {
            // LPUSH to Valkey queue (local dev / EC2 worker)
            let mut conn = s.redis.get().await?;
            conn.lpush::<_, _, ()>(format!("queue:orders:{}", req.pair_id), &order_str).await?;
            tracing::info!(
                order_id = %order_id,
                pair_id = %req.pair_id,
                user_id = %user_id,
                "order queued for processing"
            );
        }
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "order": {
                "id": order_id.to_string(),
                "status": "Pending",
                "user_id": user_id,
                "pair_id": req.pair_id,
                "side": side_str(req.side),
                "order_type": order_type_str(req.order_type),
                "tif": tif_str(tif),
                "price": req.price.map(|v| v.to_string()),
                "quantity": req.quantity.to_string(),
                "remaining": req.quantity.to_string(),
                "status": "Queued",
                "stp_mode": stp_str(stp_mode),
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

    // Check gateway-side TTL cache first (avoids Valkey round-trip)
    if let Some(cached) = s.user_cache.get(&cache_key).await {
        return Ok(raw_json(cached));
    }

    // Fallback to Valkey
    let mut conn = s.redis.get().await?;
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
    let mut conn = s.redis.get().await?;
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
    let mut conn = s.redis.get().await?;
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

    let mut conn = s.redis.get().await?;
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

    // Fallback to Valkey
    let mut conn = s.redis.get().await?;
    let cached: String = conn.get(&cache_key).await.unwrap_or_else(|_| "{}".to_string());

    if cached != "{}" {
        s.user_cache.set(cache_key, cached.clone()).await;
        return Ok(raw_json(cached));
    }

    Ok(raw_json(r#"{"balances":[]}"#))
}

// ── Dashboard API — read from CacheBroadcasts (zero Valkey) ────────────────

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

// ── GET /api/snapshot/{pair_id} ────────────────────────────────────────────────
// Returns all data for a pair in a single response — replaces 8 REST polls.

pub async fn get_snapshot(
    Path(pair_id): Path<String>,
    Query(q): Query<OrderbookQuery>,
    State(s): State<AppState>,
) -> HandlerResult<Response> {
    // Read all cache keys — zero Valkey, all from watch channels
    let ob = s.cache.get_latest(&format!("cache:orderbook:{}", pair_id));
    let trades = s.cache.get_latest(&format!("cache:trades:{}", pair_id));
    let ticker = s.cache.get_latest(&format!("cache:ticker:{}", pair_id));
    let metrics = s.cache.get_latest("cache:metrics");
    let throughput = s.cache.get_latest("cache:throughput");
    let latency = s.cache.get_latest("cache:latency_metrics");
    let pairs = s.cache.get_latest("cache:pairs");

    // P1 — Apply depth limiting to orderbook if requested
    let orderbook_json = truncate_orderbook_depth(ob.as_deref(), q.depth);

    // Build composite JSON — raw concatenation (except depth-limited orderbook)
    let mut buf = String::with_capacity(4096);
    buf.push_str(r#"{"orderbook":"#);
    buf.push_str(&orderbook_json);
    buf.push_str(r#","trades":"#);
    buf.push_str(trades.as_deref().unwrap_or(r#"{"trades":[]}"#));
    buf.push_str(r#","ticker":"#);
    buf.push_str(ticker.as_deref().unwrap_or("null"));
    buf.push_str(r#","metrics":"#);
    buf.push_str(metrics.as_deref().unwrap_or("{}"));
    buf.push_str(r#","throughput":"#);
    buf.push_str(throughput.as_deref().unwrap_or(r#"{"series":[]}"#));
    buf.push_str(r#","latency":"#);
    buf.push_str(latency.as_deref().unwrap_or("{}"));
    buf.push_str(r#","pairs":"#);
    buf.push_str(pairs.as_deref().unwrap_or(r#"{"pairs":[]}"#));
    buf.push('}');

    Ok(raw_json(buf))
}

// ── GET /api/stream/{pair_id} — SSE ───────────────────────────────────────────
// Server-Sent Events: single HTTP connection, pushes all updates for a pair.
// Replaces multiple WS connections and REST polling.

pub async fn sse_stream(
    Path(pair_id): Path<String>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let cache_keys = vec![
        (format!("cache:orderbook:{}", pair_id), "orderbook"),
        (format!("cache:trades:{}", pair_id), "trades"),
        (format!("cache:ticker:{}", pair_id), "ticker"),
        ("cache:metrics".to_string(), "metrics"),
        ("cache:throughput".to_string(), "throughput"),
        ("cache:latency_metrics".to_string(), "latency"),
    ];

    let stream = async_stream::stream! {
        // Send initial snapshot for all keys
        for (key, event_name) in &cache_keys {
            if let Some(val) = s.cache.get_latest(key) {
                yield Ok::<_, std::convert::Infallible>(Event::default().event(event_name.to_string()).data(val.to_string()));
            }
        }

        // Subscribe to all broadcast channels
        let mut receivers: Vec<(broadcast::Receiver<Arc<str>>, &str)> = Vec::new();
        for (key, event_name) in &cache_keys {
            if let Some(rx) = s.cache.subscribe(key) {
                receivers.push((rx, event_name));
            }
        }

        // Fan-in all channels
        loop {
            let mut got_msg = false;
            for (rx, event_name) in &mut receivers {
                match rx.try_recv() {
                    Ok(val) => {
                        yield Ok::<_, std::convert::Infallible>(Event::default().event(event_name.to_string()).data(val.to_string()));
                        got_msg = true;
                    }
                    Err(broadcast::error::TryRecvError::Closed) => return,
                    _ => {}
                }
            }
            if !got_msg {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    };

    Sse::new(Box::pin(stream))
        .keep_alive(axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)))
}

// ── WebSocket Feeds ───────────────────────────────────────────────────────────

/// Shared-broadcast WS handler — subscribes to a CacheBroadcasts channel
/// instead of polling Valkey directly. One Valkey poller feeds N clients.
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

// ── Multiplexed WS /ws/stream — one connection, many subscriptions ────────────
// Client sends: {"subscribe": ["orderbook:BTC-USDT", "trades:BTC-USDT", "ticker:BTC-USDT"]}
// Server pushes: {"ch": "orderbook:BTC-USDT", "data": {...}}
// One connection replaces 3+ separate WS connections.

pub async fn ws_stream(
    State(s): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_multiplexed_ws(s, socket))
}

async fn handle_multiplexed_ws(state: AppState, mut socket: WebSocket) {
    use tokio::sync::mpsc;

    // Channel for aggregating messages from all subscriptions
    let (agg_tx, mut agg_rx) = mpsc::channel::<String>(256);

    let mut subscription_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    loop {
        tokio::select! {
            // Forward aggregated messages to the client
            Some(msg) = agg_rx.recv() => {
                if socket.send(Message::Text(msg.into())).await.is_err() {
                    for h in subscription_handles { h.abort(); }
                    return;
                }
            }
            // Read client messages (subscribe commands)
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(cmd) = serde_json::from_str::<Value>(&text) {
                            if let Some(channels) = cmd.get("subscribe").and_then(|v| v.as_array()) {
                                for ch in channels {
                                    if let Some(ch_name) = ch.as_str() {
                                        let ch_name_owned = ch_name.to_string();
                                        let tx = agg_tx.clone();
                                        let state_clone = state.clone();

                                        // Handle user-specific channels (orders, portfolio)
                                        if ch_name.starts_with("orders:") {
                                            // Subscribe to order events, filter by user_id
                                            let user_id = ch_name.strip_prefix("orders:").unwrap().to_string();
                                            let mut rx = state.order_events_tx.subscribe();
                                            let handle = tokio::spawn(async move {
                                                loop {
                                                    match rx.recv().await {
                                                        Ok(msg) => {
                                                            let is_mine = serde_json::from_str::<Value>(&msg)
                                                                .ok()
                                                                .and_then(|p| Some(p.get("user_id")?.as_str()? == user_id))
                                                                .unwrap_or(false);
                                                            if is_mine {
                                                                let out = format!(r#"{{"ch":"orders:{}","data":{}}}"#, user_id, msg);
                                                                if tx.send(out).await.is_err() { return; }
                                                            }
                                                        }
                                                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                                        Err(broadcast::error::RecvError::Closed) => return,
                                                    }
                                                }
                                            });
                                            subscription_handles.push(handle);
                                        } else if ch_name.starts_with("portfolio:") {
                                            // Poll portfolio from user cache / Valkey
                                            let user_id = ch_name.strip_prefix("portfolio:").unwrap().to_string();
                                            let handle = tokio::spawn(async move {
                                                let cache_key = format!("cache:portfolio:{}", user_id);
                                                let mut last_val = String::new();
                                                loop {
                                                    // Check user cache, then Valkey
                                                    let val = if let Some(cached) = state_clone.user_cache.get(&cache_key).await {
                                                        cached
                                                    } else if let Ok(mut conn) = state_clone.redis.get().await {
                                                        use deadpool_redis::redis::AsyncCommands;
                                                        conn.get::<_, String>(&cache_key).await.unwrap_or_default()
                                                    } else {
                                                        String::new()
                                                    };
                                                    if !val.is_empty() && val != "{}" && val != last_val {
                                                        let msg = format!(r#"{{"ch":"portfolio:{}","data":{}}}"#, user_id, val);
                                                        if tx.send(msg).await.is_err() { return; }
                                                        last_val = val;
                                                    }
                                                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                                }
                                            });
                                            subscription_handles.push(handle);
                                        } else {
                                            // Standard cache broadcast channel
                                            let cache_key = format!("cache:{}", ch_name);

                                            // Send current value immediately
                                            if let Some(val) = state.cache.get_latest(&cache_key) {
                                                let init = format!(r#"{{"ch":"{}","data":{}}}"#, ch_name_owned, val);
                                                let _ = tx.send(init).await;
                                            }

                                            // Spawn a task to forward updates
                                            let handle = tokio::spawn(async move {
                                                if let Some(mut rx) = state_clone.cache.subscribe(&cache_key) {
                                                    loop {
                                                        match rx.recv().await {
                                                            Ok(val) => {
                                                                let msg = format!(r#"{{"ch":"{}","data":{}}}"#, ch_name_owned, val);
                                                                if tx.send(msg).await.is_err() {
                                                                    return;
                                                                }
                                                            }
                                                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                                            Err(broadcast::error::RecvError::Closed) => return,
                                                        }
                                                    }
                                                }
                                            });
                                            subscription_handles.push(handle);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = socket.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        for h in subscription_handles { h.abort(); }
                        return;
                    }
                    _ => {}
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

// ── Internal manage endpoint (deploy / admin use only) ────────────────────────
//
// POST /internal/manage {"command": "...", ...args}
//
// Commands:
//   run_migrations          — run sqlx migrations (embedded in binary)
//   reset_all               — truncate orders/trades + reset balances for N users
//   reset_balances          — reset balances only (no order truncation)
//   truncate_orders         — truncate orders and trades tables + clear book cache
//   exec_sql                — execute arbitrary SQL (admin only)
//   query                   — SELECT query, returns rows as JSON array

#[derive(serde::Deserialize)]
pub struct ManageRequest {
    pub command: String,
    pub sql: Option<String>,
    pub num_users: Option<i64>,
    /// Alias used by deploy.sh / bench scripts
    pub users: Option<i64>,
}

pub async fn internal_manage(
    axum::Json(req): axum::Json<ManageRequest>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::Json;
    use serde_json::json;

    match req.command.as_str() {
        // ── run_migrations: embedded migrations, no Valkey needed ────────────
        "run_migrations" => {
            tracing::info!("manage: run_migrations");
            let database_url = match std::env::var("DATABASE_URL") {
                Ok(u) => u,
                Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "DATABASE_URL not set"}))).into_response(),
            };
            let pg = match sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(std::time::Duration::from_secs(30))
                .connect(&database_url)
                .await
            {
                Ok(p) => p,
                Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("PG connect failed: {e}")}))).into_response(),
            };
            match sqlx::migrate!("../../migrations").run(&pg).await {
                Ok(_) => {
                    tracing::info!("manage: migrations complete");
                    (StatusCode::OK, Json(json!({"status": "ok", "command": "run_migrations"}))).into_response()
                }
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("migrations failed: {e}")}))).into_response(),
            }
        }

        // ── Commands that need full worker state (Valkey + PG) ───────────────
        cmd => {
            let state = match crate::worker::get_state().await {
                Ok(s) => s,
                Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("state init failed: {e}")}))).into_response(),
            };

            match cmd {
                "reset_all" => {
                    let n = req.users.or(req.num_users).unwrap_or(100) as i32;
                    tracing::info!(n, "manage: reset_all");
                    if let Err(e) = sqlx::query("TRUNCATE orders CASCADE")
                        .execute(&state.pg).await
                    {
                        return (StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": e.to_string()}))).into_response();
                    }
                    for i in 1..=n {
                        let user = format!("user-{i}");
                        let r1 = sqlx::query(
                            "INSERT INTO balances (user_id, asset, available, locked) VALUES ($1, 'BTC', 10, 0) \
                             ON CONFLICT (user_id, asset) DO UPDATE SET available = 10, locked = 0"
                        ).bind(&user).execute(&state.pg).await;
                        let r2 = sqlx::query(
                            "INSERT INTO balances (user_id, asset, available, locked) VALUES ($1, 'USDT', 1000000, 0) \
                             ON CONFLICT (user_id, asset) DO UPDATE SET available = 1000000, locked = 0"
                        ).bind(&user).execute(&state.pg).await;
                        if let Err(e) = r1.and(r2) {
                            return (StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({"error": e.to_string()}))).into_response();
                        }
                    }
                    if let Err(e) = sme_shared::cache::init_balances_from_pg(&state.redis, &state.pg).await {
                        return (StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": format!("balance cache reinit failed: {e}")}))).into_response();
                    }
                    // Flush live book + cache keys for all pairs
                    if let Ok(mut conn) = state.redis.get().await {
                        for pair in &["BTC-USDT", "ETH-USDT", "SOL-USDT"] {
                            let _: () = deadpool_redis::redis::cmd("DEL")
                                .arg(format!("book:{}:bids", pair))
                                .arg(format!("book:{}:asks", pair))
                                .arg(format!("version:{}", pair))
                                .arg(format!("cache:orderbook:{}", pair))
                                .arg(format!("cache:trades:{}", pair))
                                .arg(format!("cache:ticker:{}", pair))
                                .arg(format!("queue:orders:{}", pair))
                                .query_async(&mut *conn).await.unwrap_or(());
                        }
                    }
                    tracing::info!(n, "manage: reset_all complete");
                    (StatusCode::OK, Json(json!({"ok": true, "users": n}))).into_response()
                }

                "reset_balances" => {
                    let n = req.users.or(req.num_users).unwrap_or(100) as i32;
                    tracing::info!(n, "manage: reset_balances");
                    for i in 1..=n {
                        let user = format!("user-{i}");
                        let r1 = sqlx::query(
                            "INSERT INTO balances (user_id, asset, available, locked) VALUES ($1, 'BTC', 10, 0) \
                             ON CONFLICT (user_id, asset) DO UPDATE SET available = 10, locked = 0"
                        ).bind(&user).execute(&state.pg).await;
                        let r2 = sqlx::query(
                            "INSERT INTO balances (user_id, asset, available, locked) VALUES ($1, 'USDT', 1000000, 0) \
                             ON CONFLICT (user_id, asset) DO UPDATE SET available = 1000000, locked = 0"
                        ).bind(&user).execute(&state.pg).await;
                        if let Err(e) = r1.and(r2) {
                            return (StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({"error": e.to_string()}))).into_response();
                        }
                    }
                    if let Err(e) = sme_shared::cache::init_balances_from_pg(&state.redis, &state.pg).await {
                        return (StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": format!("balance cache reinit failed: {e}")}))).into_response();
                    }
                    tracing::info!(n, "manage: reset_balances complete");
                    (StatusCode::OK, Json(json!({"status": "ok", "command": "reset_balances", "users": n}))).into_response()
                }

                // Scan book sorted sets for entries whose order hash has expired,
                // remove them. Safe to run anytime — won't touch live orders.
                "sweep_stale_orders" => {
                    tracing::info!("manage: sweep_stale_orders");
                    let mut removed_total: u64 = 0;
                    if let Ok(mut conn) = state.redis.get().await {
                        for pair in &["BTC-USDT", "ETH-USDT", "SOL-USDT"] {
                            for side in &["bids", "asks"] {
                                let book_key = format!("book:{}:{}", pair, side);
                                // Get all order IDs in the book
                                let members: Vec<String> = deadpool_redis::redis::cmd("ZRANGE")
                                    .arg(&book_key).arg("0").arg("-1")
                                    .query_async(&mut *conn).await.unwrap_or_default();
                                for order_id in members {
                                    let order_key = format!("order:{}", order_id);
                                    let exists: bool = deadpool_redis::redis::cmd("EXISTS")
                                        .arg(&order_key)
                                        .query_async(&mut *conn).await.unwrap_or(false);
                                    if !exists {
                                        let _: () = deadpool_redis::redis::cmd("ZREM")
                                            .arg(&book_key).arg(&order_id)
                                            .query_async(&mut *conn).await.unwrap_or(());
                                        removed_total += 1;
                                    }
                                }
                            }
                        }
                    }
                    tracing::info!(removed = removed_total, "manage: sweep_stale_orders complete");
                    (StatusCode::OK, Json(json!({"ok": true, "removed": removed_total}))).into_response()
                }

                "truncate_orders" => {
                    tracing::info!("manage: truncate_orders");
                    if let Err(e) = sqlx::query("TRUNCATE orders CASCADE")
                        .execute(&state.pg).await
                    {
                        return (StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": e.to_string()}))).into_response();
                    }
                    if let Ok(mut conn) = state.redis.get().await {
                        for pair in &["BTC-USDT", "ETH-USDT", "SOL-USDT"] {
                            let _: () = deadpool_redis::redis::cmd("DEL")
                                .arg(format!("book:{}:bids", pair))
                                .arg(format!("book:{}:asks", pair))
                                .arg(format!("version:{}", pair))
                                .arg(format!("cache:orderbook:{}", pair))
                                .arg(format!("cache:trades:{}", pair))
                                .arg(format!("cache:ticker:{}", pair))
                                .arg(format!("queue:orders:{}", pair))
                                .query_async(&mut *conn).await.unwrap_or(());
                        }
                    }
                    tracing::info!("manage: truncate_orders complete");
                    (StatusCode::OK, Json(json!({"status": "ok", "command": "truncate_orders"}))).into_response()
                }

                "exec_sql" => {
                    let sql = match req.sql.as_deref() {
                        Some(s) => s,
                        None => return (StatusCode::BAD_REQUEST,
                            Json(json!({"error": "sql argument required"}))).into_response(),
                    };
                    tracing::info!(sql, "manage: exec_sql");
                    match sqlx::query(sql).execute(&state.pg).await {
                        Ok(r) => (StatusCode::OK, Json(json!({"status": "ok", "command": "exec_sql", "rows_affected": r.rows_affected()}))).into_response(),
                        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": e.to_string()}))).into_response(),
                    }
                }

                "query" => {
                    let sql = match req.sql.as_deref() {
                        Some(s) => s,
                        None => return (StatusCode::BAD_REQUEST,
                            Json(json!({"error": "sql required"}))).into_response(),
                    };
                    match sqlx::query(sql).fetch_all(&state.pg).await {
                        Ok(rows) => {
                            let result: Vec<serde_json::Value> = rows.iter().map(|row| {
                                use sqlx::Row;
                                use sqlx::Column;
                                let mut map = serde_json::Map::new();
                                for (i, col) in row.columns().iter().enumerate() {
                                    let val: serde_json::Value =
                                        if let Ok(v) = row.try_get::<Option<i64>, _>(i) {
                                            v.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null)
                                        } else if let Ok(v) = row.try_get::<Option<f64>, _>(i) {
                                            v.map(|f| serde_json::json!(f)).unwrap_or(serde_json::Value::Null)

                                        } else if let Ok(v) = row.try_get::<Option<bool>, _>(i) {
                                            v.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null)
                                        } else if let Ok(v) = row.try_get::<Option<String>, _>(i) {
                                            v.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null)
                                        } else {
                                            serde_json::Value::Null
                                        };
                                    map.insert(col.name().to_string(), val);
                                }
                                serde_json::Value::Object(map)
                            }).collect();
                            (StatusCode::OK, Json(json!({"rows": result}))).into_response()
                        }
                        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": e.to_string()}))).into_response(),
                    }
                }

                _ => (StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("unknown command: {}", cmd)}))).into_response(),
            }
        }
    }
}
