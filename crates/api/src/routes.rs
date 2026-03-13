//! API route handlers — placeholder implementations.

use axum::extract::{Path, WebSocketUpgrade, ws::WebSocket};
use axum::response::{IntoResponse, Json};
use serde_json::json;
use uuid::Uuid;

// ── Trading API ──────────────────────────────────────────────────────────────

pub async fn list_pairs() -> impl IntoResponse {
    // TODO: fetch from DB
    Json(json!({ "pairs": [] }))
}

pub async fn get_orderbook(Path(pair_id): Path<String>) -> impl IntoResponse {
    // TODO: read from Dragonfly sorted sets
    Json(json!({ "pair": pair_id, "bids": [], "asks": [] }))
}

pub async fn get_trades(Path(pair_id): Path<String>) -> impl IntoResponse {
    // TODO: recent trades from DB/cache
    Json(json!({ "pair": pair_id, "trades": [] }))
}

pub async fn get_ticker(Path(pair_id): Path<String>) -> impl IntoResponse {
    // TODO: compute from cache
    Json(json!({ "pair": pair_id, "last": null, "high_24h": null, "low_24h": null, "volume_24h": null }))
}

pub async fn create_order(body: String) -> impl IntoResponse {
    // TODO: parse order, validate (tick/lot/size/price band), publish to stream
    Json(json!({ "status": "accepted", "order_id": Uuid::new_v4() }))
}

pub async fn cancel_order(Path(order_id): Path<Uuid>) -> impl IntoResponse {
    // TODO: cancel resting order
    Json(json!({ "status": "canceled", "order_id": order_id }))
}

pub async fn modify_order(Path(order_id): Path<Uuid>, body: String) -> impl IntoResponse {
    // TODO: cancel-replace resting order
    Json(json!({ "status": "modified", "order_id": order_id }))
}

pub async fn list_orders() -> impl IntoResponse {
    // TODO: user open orders
    Json(json!({ "orders": [] }))
}

pub async fn get_portfolio() -> impl IntoResponse {
    // TODO: user balances per asset
    Json(json!({ "balances": [] }))
}

// ── WebSocket Feeds ──────────────────────────────────────────────────────────

pub async fn ws_orderbook(Path(pair_id): Path<String>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_orderbook_ws(pair_id, socket))
}

async fn handle_orderbook_ws(pair_id: String, mut socket: WebSocket) {
    // TODO: subscribe to order book updates, push to client
    let _ = pair_id;
}

pub async fn ws_trades(Path(pair_id): Path<String>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_trades_ws(pair_id, socket))
}

async fn handle_trades_ws(pair_id: String, mut socket: WebSocket) {
    // TODO: subscribe to trade feed, push to client
    let _ = pair_id;
}

// ── Dashboard API ────────────────────────────────────────────────────────────

pub async fn get_metrics() -> impl IntoResponse {
    // TODO: aggregate from Dragonfly metric keys
    Json(json!({
        "orders_per_sec": 0,
        "matches_per_sec": 0,
        "trades_per_sec": 0,
        "active_pairs": 0,
        "active_workers": 0
    }))
}

pub async fn get_lock_metrics() -> impl IntoResponse {
    Json(json!({
        "contention_rate": 0.0,
        "avg_wait_ms": 0.0,
        "retry_count": 0,
        "failures": 0
    }))
}

pub async fn get_throughput() -> impl IntoResponse {
    // TODO: time-series throughput data
    Json(json!({ "series": [] }))
}
