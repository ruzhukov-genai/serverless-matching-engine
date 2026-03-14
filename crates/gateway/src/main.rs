use anyhow::Result;
use axum::{Router, routing::get};
use deadpool_redis::Pool as RedisPool;
use std::sync::Arc;
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

mod routes;

#[derive(Clone)]
pub struct AppState {
    pub dragonfly: RedisPool,
    /// Order events broadcast — single channel, WS clients filter by user_id
    pub order_events_tx: broadcast::Sender<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    tracing::info!("sme-gateway starting");

    let config = sme_shared::Config::from_env();
    let dragonfly = sme_shared::cache::create_pool(&config.dragonfly_url).await?;

    // Order events broadcast — single channel, WS clients filter by user_id
    let (order_events_tx, _) = broadcast::channel::<String>(1024);

    let state = AppState {
        dragonfly,
        order_events_tx,
    };

    let app = Router::new()
        // Trading API - all read from Dragonfly cache
        .route("/api/pairs", get(routes::list_pairs))
        .route("/api/orderbook/{pair_id}", get(routes::get_orderbook))
        .route("/api/trades/{pair_id}", get(routes::get_trades))
        .route("/api/ticker/{pair_id}", get(routes::get_ticker))
        .route("/api/orders", get(routes::list_orders).post(routes::create_order).delete(routes::cancel_all_orders))
        .route(
            "/api/orders/{order_id}",
            axum::routing::delete(routes::cancel_order).put(routes::modify_order),
        )
        .route("/api/portfolio", get(routes::get_portfolio))
        // WebSocket feeds
        .route("/ws/orderbook/{pair_id}", get(routes::ws_orderbook))
        .route("/ws/trades/{pair_id}", get(routes::ws_trades))
        .route("/ws/orders/{user_id}", get(routes::ws_orders))
        // Dashboard API - all read from Dragonfly cache
        .route("/api/metrics", get(routes::get_metrics))
        .route("/api/metrics/locks", get(routes::get_lock_metrics))
        .route("/api/metrics/throughput", get(routes::get_throughput))
        .route("/api/metrics/latency", get(routes::get_latency_percentiles))
        .route("/api/metrics/audit", get(routes::get_audit))
        .route("/api/audit", get(routes::get_audit))
        // Serve static files
        .nest_service("/trading", ServeDir::new("web/trading"))
        .nest_service("/dashboard", ServeDir::new("web/dashboard"))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let port = std::env::var("PORT").unwrap_or_else(|_| "3001".to_string());
    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on http://{}", addr);
    axum::serve(listener, app).await?;

    Ok(())
}