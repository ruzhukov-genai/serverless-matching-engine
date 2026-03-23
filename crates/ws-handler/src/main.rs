//! WebSocket Handler Lambda — handles API Gateway WebSocket events.
//!
//! Invoked directly by API Gateway WebSocket API (no Lambda Web Adapter).
//! Handles $connect, $disconnect, and $default (subscribe/unsubscribe commands).
//!
//! Connection state is stored in Dragonfly:
//!   ws:connections          → SET of all active connectionIds
//!   ws:subs:{channel}       → SET of connectionIds subscribed to this channel
//!   ws:conn:{connectionId}  → SET of channels this connection is subscribed to
//!
//! On subscribe, the current cached value is sent immediately via API GW Management API.
//! Stale connections (410 Gone) are removed from Dragonfly on push failure.

use std::env;
use std::time::Duration;

use anyhow::Result;
use deadpool_redis::{Config, Pool, Runtime};
use lambda_runtime::{Error as LambdaError, LambdaEvent, run, service_fn};
use once_cell::sync::OnceCell;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Event types from API GW WebSocket ────────────────────────────────────────

#[derive(Deserialize, Debug)]
struct WsEvent {
    #[serde(rename = "requestContext")]
    request_context: RequestContext,
    body: Option<String>,
}

#[derive(Deserialize, Debug)]
struct RequestContext {
    #[serde(rename = "routeKey")]
    route_key: String,
    #[serde(rename = "connectionId")]
    connection_id: String,
    #[serde(rename = "domainName")]
    domain_name: Option<String>,
    stage: Option<String>,
}

#[derive(Serialize)]
struct WsResponse {
    #[serde(rename = "statusCode")]
    status_code: u16,
    body: Option<String>,
}

// ── Singleton state (warm-start reuse) ───────────────────────────────────────

struct AppState {
    pool: Pool,
}

static STATE: OnceCell<AppState> = OnceCell::new();

async fn get_state() -> Result<&'static AppState> {
    if let Some(s) = STATE.get() {
        return Ok(s);
    }

    let dragonfly_url = env::var("DRAGONFLY_URL")
        .unwrap_or_else(|_| "redis://localhost:6379".to_string());

    let mut cfg = Config::from_url(&dragonfly_url);
    cfg.pool = Some(deadpool_redis::PoolConfig {
        max_size: 5,
        timeouts: deadpool_redis::Timeouts {
            wait: Some(Duration::from_secs(5)),
            create: Some(Duration::from_secs(5)),
            recycle: Some(Duration::from_secs(5)),
        },
        queue_mode: Default::default(),
    });

    let pool = cfg.create_pool(Some(Runtime::Tokio1))
        .map_err(|e| anyhow::anyhow!("failed to create redis pool: {}", e))?;

    let state = AppState { pool };
    let _ = STATE.set(state);
    Ok(STATE.get().expect("just set"))
}

// ── Lambda handler ────────────────────────────────────────────────────────────

async fn handler(event: LambdaEvent<WsEvent>) -> Result<WsResponse, LambdaError> {
    let state = get_state().await.map_err(|e| LambdaError::from(e.to_string()))?;
    let (ws_event, _ctx) = event.into_parts();

    handle_ws_event(state, ws_event)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "ws handler error");
            LambdaError::from(e.to_string())
        })
}

async fn handle_ws_event(state: &AppState, event: WsEvent) -> Result<WsResponse> {
    let conn_id = &event.request_context.connection_id;
    let route = &event.request_context.route_key;

    tracing::info!(route = %route, connection_id = %conn_id, "ws event");

    match route.as_str() {
        "$connect" => handle_connect(state, conn_id).await,
        "$disconnect" => handle_disconnect(state, conn_id).await,
        _ => handle_default(state, &event).await,
    }
}

// ── Route handlers ────────────────────────────────────────────────────────────

async fn handle_connect(state: &AppState, conn_id: &str) -> Result<WsResponse> {
    let mut conn = state.pool.get().await?;
    conn.sadd::<_, _, ()>("ws:connections", conn_id).await?;
    // TTL on connection membership — auto-cleanup if disconnect event is missed
    conn.expire::<_, ()>("ws:connections", 86400).await?;
    tracing::info!(connection_id = %conn_id, "client connected");
    Ok(WsResponse { status_code: 200, body: None })
}

async fn handle_disconnect(state: &AppState, conn_id: &str) -> Result<WsResponse> {
    let mut conn = state.pool.get().await?;

    // Remove from global connections set
    conn.srem::<_, _, ()>("ws:connections", conn_id).await?;

    // Get all channels this connection was subscribed to
    let channels: Vec<String> = conn
        .smembers(format!("ws:conn:{}", conn_id))
        .await
        .unwrap_or_default();

    // Remove from each channel's subscriber set
    for ch in &channels {
        conn.srem::<_, _, ()>(format!("ws:subs:{}", ch), conn_id).await?;
    }

    // Remove reverse mapping
    conn.del::<_, ()>(format!("ws:conn:{}", conn_id)).await?;

    tracing::info!(
        connection_id = %conn_id,
        channels = channels.len(),
        "client disconnected"
    );
    Ok(WsResponse { status_code: 200, body: None })
}

async fn handle_default(state: &AppState, event: &WsEvent) -> Result<WsResponse> {
    let conn_id = &event.request_context.connection_id;

    let body = match &event.body {
        Some(b) if !b.is_empty() => b,
        _ => return Ok(WsResponse { status_code: 200, body: None }),
    };

    let cmd: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => {
            tracing::warn!(connection_id = %conn_id, body = %body, "unparseable ws message");
            return Ok(WsResponse { status_code: 400, body: Some("invalid JSON".to_string()) });
        }
    };

    // Handle subscribe command: {"action": "...", "subscribe": ["trades:BTC-USDT", ...]}
    if let Some(channels) = cmd.get("subscribe").and_then(|v| v.as_array()) {
        let domain = event.request_context.domain_name.as_deref().unwrap_or("");
        let stage = event.request_context.stage.as_deref().unwrap_or("ws");
        let apigw_endpoint = format!("https://{}/{}", domain, stage);

        handle_subscribe(state, conn_id, channels, &apigw_endpoint).await?;
    }

    // Handle unsubscribe command: {"action": "...", "unsubscribe": ["trades:BTC-USDT", ...]}
    if let Some(channels) = cmd.get("unsubscribe").and_then(|v| v.as_array()) {
        handle_unsubscribe(state, conn_id, channels).await?;
    }

    Ok(WsResponse { status_code: 200, body: None })
}

async fn handle_subscribe(
    state: &AppState,
    conn_id: &str,
    channels: &[Value],
    apigw_endpoint: &str,
) -> Result<()> {
    let mut conn = state.pool.get().await?;

    // Build API GW management client for sending initial snapshot values
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let apigw_config = aws_sdk_apigatewaymanagement::config::Builder::from(&config)
        .endpoint_url(apigw_endpoint)
        .build();
    let apigw = aws_sdk_apigatewaymanagement::Client::from_conf(apigw_config);

    for ch in channels {
        let ch_name = match ch.as_str() {
            Some(s) => s,
            None => continue,
        };

        // Validate channel name format (prevent injection)
        if !is_valid_channel(ch_name) {
            tracing::warn!(channel = %ch_name, "invalid channel name");
            continue;
        }

        // Register subscription (bidirectional)
        conn.sadd::<_, _, ()>(format!("ws:subs:{}", ch_name), conn_id).await?;
        conn.sadd::<_, _, ()>(format!("ws:conn:{}", conn_id), ch_name).await?;

        // TTL: 1 hour — auto-cleanup stale subscription state
        conn.expire::<_, ()>(format!("ws:conn:{}", conn_id), 3600).await?;
        conn.expire::<_, ()>(format!("ws:subs:{}", ch_name), 3600).await?;

        // Send current cached value immediately (snapshot-on-subscribe)
        let cache_key = format!("cache:{}", ch_name);
        if let Ok(val) = conn.get::<_, String>(&cache_key).await {
            if !val.is_empty() {
                let msg = format!(r#"{{"ch":"{}","data":{}}}"#, ch_name, val);
                let result = apigw
                    .post_to_connection()
                    .connection_id(conn_id)
                    .data(aws_sdk_apigatewaymanagement::primitives::Blob::new(msg.into_bytes()))
                    .send()
                    .await;

                if let Err(e) = result {
                    tracing::warn!(
                        connection_id = %conn_id,
                        channel = %ch_name,
                        error = %e,
                        "failed to send snapshot"
                    );
                }
            }
        }

        tracing::info!(connection_id = %conn_id, channel = %ch_name, "subscribed");
    }

    Ok(())
}

async fn handle_unsubscribe(
    state: &AppState,
    conn_id: &str,
    channels: &[Value],
) -> Result<()> {
    let mut conn = state.pool.get().await?;

    for ch in channels {
        let ch_name = match ch.as_str() {
            Some(s) => s,
            None => continue,
        };

        conn.srem::<_, _, ()>(format!("ws:subs:{}", ch_name), conn_id).await?;
        conn.srem::<_, _, ()>(format!("ws:conn:{}", conn_id), ch_name).await?;

        tracing::info!(connection_id = %conn_id, channel = %ch_name, "unsubscribed");
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Validate channel names to prevent key injection.
/// Valid: "orderbook:BTC-USDT", "trades:ETH-USDT", "ticker:BTC-USDT"
fn is_valid_channel(ch: &str) -> bool {
    if ch.len() > 64 {
        return false;
    }
    ch.chars().all(|c| c.is_alphanumeric() || c == ':' || c == '-' || c == '_')
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), LambdaError> {
    tracing_subscriber::fmt()
        .with_env_filter(env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .json()
        .without_time() // Lambda adds its own timestamp
        .init();

    tracing::info!("sme-ws-handler starting");

    run(service_fn(handler)).await
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_channel() {
        assert!(is_valid_channel("orderbook:BTC-USDT"));
        assert!(is_valid_channel("trades:ETH-USDT"));
        assert!(is_valid_channel("ticker:SOL-USDT"));
        assert!(!is_valid_channel("../etc/passwd"));
        assert!(!is_valid_channel("trades:BTC USDT")); // space
        assert!(!is_valid_channel(&"a".repeat(65)));   // too long
        assert!(is_valid_channel(&"a".repeat(64)));    // max length ok
    }

    #[test]
    fn test_ws_response_serialization() {
        let r = WsResponse { status_code: 200, body: None };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("statusCode"));
        assert!(s.contains("200"));
    }

    #[test]
    fn test_ws_event_deserialization() {
        let raw = r#"{
            "requestContext": {
                "routeKey": "$connect",
                "connectionId": "abc123",
                "domainName": "abc.execute-api.us-east-1.amazonaws.com",
                "stage": "ws"
            },
            "body": null
        }"#;
        let event: WsEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(event.request_context.route_key, "$connect");
        assert_eq!(event.request_context.connection_id, "abc123");
        assert!(event.body.is_none());
    }

    #[test]
    fn test_subscribe_event_deserialization() {
        let raw = r#"{
            "requestContext": {
                "routeKey": "$default",
                "connectionId": "xyz789",
                "domainName": "abc.execute-api.us-east-1.amazonaws.com",
                "stage": "ws"
            },
            "body": "{\"action\":\"subscribe\",\"subscribe\":[\"orderbook:BTC-USDT\",\"trades:BTC-USDT\"]}"
        }"#;
        let event: WsEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(event.request_context.route_key, "$default");
        let body = event.body.unwrap();
        let cmd: Value = serde_json::from_str(&body).unwrap();
        let channels = cmd.get("subscribe").unwrap().as_array().unwrap();
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].as_str().unwrap(), "orderbook:BTC-USDT");
    }
}
