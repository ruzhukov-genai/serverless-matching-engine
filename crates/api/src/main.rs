use anyhow::Result;
use axum::{Router, routing::get};
use deadpool_redis::Pool as RedisPool;
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

mod routes;

/// Cached pair configuration — loaded once at startup, never re-queried.
/// Eliminates the SELECT from pairs on every order validation.
#[derive(Clone, Debug)]
pub struct PairConfig {
    pub tick_size: Decimal,
    pub lot_size: Decimal,
    pub min_order_size: Decimal,
    pub max_order_size: Decimal,
    pub active: bool,
}

#[derive(Clone)]
pub struct AppState {
    pub dragonfly: RedisPool,
    pub pg: PgPool,
    /// In-memory pairs cache: pair_id → PairConfig
    pub pairs_cache: Arc<HashMap<String, PairConfig>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    tracing::info!("sme-api starting");

    let config = sme_shared::Config::from_env();
    let dragonfly = sme_shared::cache::create_pool(&config.dragonfly_url).await?;
    let pg = sme_shared::db::create_pool(&config.database_url).await?;

    tracing::info!("running migrations");
    sme_shared::db::run_migrations(&pg).await?;

    // Run seed SQL directly
    tracing::info!("seeding initial data");
    run_seed(&pg).await?;

    // Load pairs into memory cache — eliminates SELECT on every order validation
    tracing::info!("loading pairs cache");
    let pairs_cache = Arc::new(load_pairs_cache(&pg).await?);
    tracing::info!(count = pairs_cache.len(), "pairs cache loaded");

    let state = AppState { dragonfly, pg, pairs_cache };

    let app = Router::new()
        // Trading API
        .route("/api/pairs", get(routes::list_pairs))
        .route("/api/orderbook/{pair_id}", get(routes::get_orderbook))
        .route("/api/trades/{pair_id}", get(routes::get_trades))
        .route("/api/ticker/{pair_id}", get(routes::get_ticker))
        .route("/api/orders", get(routes::list_orders).post(routes::create_order))
        .route(
            "/api/orders/{order_id}",
            axum::routing::delete(routes::cancel_order).put(routes::modify_order),
        )
        .route("/api/portfolio", get(routes::get_portfolio))
        // WebSocket feeds
        .route("/ws/orderbook/{pair_id}", get(routes::ws_orderbook))
        .route("/ws/trades/{pair_id}", get(routes::ws_trades))
        // Dashboard API
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

async fn load_pairs_cache(pg: &PgPool) -> Result<HashMap<String, PairConfig>> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT id, tick_size, lot_size, min_order_size, max_order_size, active FROM pairs",
    )
    .fetch_all(pg)
    .await?;

    let mut map = HashMap::with_capacity(rows.len());
    for row in rows {
        let id: String = row.get("id");
        map.insert(id, PairConfig {
            tick_size: row.get("tick_size"),
            lot_size: row.get("lot_size"),
            min_order_size: row.get("min_order_size"),
            max_order_size: row.get("max_order_size"),
            active: row.get("active"),
        });
    }
    Ok(map)
}

async fn run_seed(pg: &PgPool) -> Result<()> {
    use rust_decimal::Decimal;
    use std::str::FromStr;

    // Pairs
    for (id, base, quote, tick, lot, mn, mx, pp, qp) in [
        ("BTC-USDT", "BTC", "USDT", "0.01", "0.00001", "0.00001", "100", 2i16, 5i16),
        ("ETH-USDT", "ETH", "USDT", "0.01", "0.0001", "0.0001", "1000", 2, 4),
        ("SOL-USDT", "SOL", "USDT", "0.001", "0.01", "0.01", "10000", 3, 2),
    ] {
        let tick_d = Decimal::from_str(tick).unwrap();
        let lot_d = Decimal::from_str(lot).unwrap();
        let mn_d = Decimal::from_str(mn).unwrap();
        let mx_d = Decimal::from_str(mx).unwrap();
        sqlx::query(
            "INSERT INTO pairs (id, base, quote, tick_size, lot_size, min_order_size, max_order_size, price_precision, qty_precision, price_band_pct, active)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,0.10,true) ON CONFLICT DO NOTHING",
        )
        .bind(id).bind(base).bind(quote)
        .bind(tick_d).bind(lot_d).bind(mn_d).bind(mx_d)
        .bind(pp).bind(qp)
        .execute(pg).await?;
    }

    // Balances
    for (user, asset, avail) in [
        ("user-1", "BTC", "10"), ("user-1", "ETH", "100"), ("user-1", "SOL", "1000"), ("user-1", "USDT", "1000000"),
        ("user-2", "BTC", "10"), ("user-2", "ETH", "100"), ("user-2", "SOL", "1000"), ("user-2", "USDT", "1000000"),
    ] {
        let avail_d = Decimal::from_str(avail).unwrap();
        sqlx::query(
            "INSERT INTO balances (user_id, asset, available, locked) VALUES ($1,$2,$3,0) ON CONFLICT DO NOTHING",
        )
        .bind(user).bind(asset).bind(avail_d)
        .execute(pg).await?;
    }

    Ok(())
}
