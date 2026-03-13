use anyhow::Result;
use axum::{Router, routing::get};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

mod routes;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    tracing::info!("sme-api starting");

    // TODO: initialize Dragonfly + PostgreSQL connection pools

    let app = Router::new()
        // Trading API
        .route("/api/pairs", get(routes::list_pairs))
        .route("/api/orderbook/{pair_id}", get(routes::get_orderbook))
        .route("/api/trades/{pair_id}", get(routes::get_trades))
        .route("/api/ticker/{pair_id}", get(routes::get_ticker))
        .route("/api/orders", get(routes::list_orders).post(routes::create_order))
        .route("/api/orders/{order_id}", axum::routing::delete(routes::cancel_order).put(routes::modify_order))
        .route("/api/portfolio", get(routes::get_portfolio))
        // WebSocket feeds
        .route("/ws/orderbook/{pair_id}", get(routes::ws_orderbook))
        .route("/ws/trades/{pair_id}", get(routes::ws_trades))
        // Dashboard API
        .route("/api/metrics", get(routes::get_metrics))
        .route("/api/metrics/locks", get(routes::get_lock_metrics))
        .route("/api/metrics/throughput", get(routes::get_throughput))
        // Serve static files
        .nest_service("/trading", ServeDir::new("web/trading"))
        .nest_service("/dashboard", ServeDir::new("web/dashboard"))
        .layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    tracing::info!("listening on http://0.0.0.0:3000");
    axum::serve(listener, app).await?;

    Ok(())
}
