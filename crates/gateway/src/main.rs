use anyhow::Result;
use axum::{Router, routing::get};
use deadpool_redis::Pool as RedisPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

mod routes;

/// Shared cache broadcasts — one poller per key, N subscribers.
/// REST handlers read from `latest` (zero Dragonfly ops).
/// WS handlers subscribe to the broadcast channel.
#[derive(Clone)]
pub struct CacheBroadcasts {
    senders: Arc<HashMap<String, broadcast::Sender<String>>>,
    latest: Arc<RwLock<HashMap<String, String>>>,
}

impl CacheBroadcasts {
    /// Subscribe to updates for a cache key. Returns None if the key is unknown.
    pub fn subscribe(&self, key: &str) -> Option<broadcast::Receiver<String>> {
        self.senders.get(key).map(|tx| tx.subscribe())
    }

    /// Get the last polled value for a cache key (zero Dragonfly, reads RAM).
    pub async fn get_latest(&self, key: &str) -> Option<String> {
        self.latest.read().await.get(key).cloned()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub dragonfly: RedisPool,
    /// Order events broadcast — single channel, WS clients filter by user_id
    pub order_events_tx: broadcast::Sender<String>,
    /// Shared cache broadcasts — one poller per key, N subscribers
    pub cache: CacheBroadcasts,
}

/// Background poller: reads one key from Dragonfly at `interval_ms`, broadcasts to all subscribers,
/// and stores the latest value in the shared `latest` map.
async fn spawn_cache_poller(
    pool: RedisPool,
    key: String,
    tx: broadcast::Sender<String>,
    latest: Arc<RwLock<HashMap<String, String>>>,
    interval_ms: u64,
) {
    use deadpool_redis::redis::AsyncCommands;
    use tokio::time::{Duration, interval};

    let mut ticker = interval(Duration::from_millis(interval_ms));
    loop {
        ticker.tick().await;
        if let Ok(mut conn) = pool.get().await {
            if let Ok(val) = conn.get::<_, String>(&key).await {
                if !val.is_empty() && val != "{}" {
                    latest.write().await.insert(key.clone(), val.clone());
                    let _ = tx.send(val);
                }
            }
        }
    }
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

    // ── Build CacheBroadcasts ─────────────────────────────────────────────────
    let pairs = ["BTC-USDT", "ETH-USDT", "SOL-USDT"];

    // Define all polled cache keys and their intervals (ms)
    let mut poll_specs: Vec<(String, u64)> = Vec::new();
    for pair in &pairs {
        poll_specs.push((format!("cache:orderbook:{}", pair), 500));
        poll_specs.push((format!("cache:trades:{}", pair), 500));
        poll_specs.push((format!("cache:ticker:{}", pair), 1000));
    }
    for key in &["cache:metrics", "cache:lock_metrics", "cache:throughput", "cache:latency_metrics", "cache:audit"] {
        poll_specs.push((key.to_string(), 2000));
    }
    poll_specs.push(("cache:pairs".to_string(), 5000));

    let latest: Arc<RwLock<HashMap<String, String>>> = Arc::new(RwLock::new(HashMap::new()));
    let mut senders: HashMap<String, broadcast::Sender<String>> = HashMap::new();

    for (key, interval_ms) in &poll_specs {
        let (tx, _) = broadcast::channel::<String>(64);
        senders.insert(key.clone(), tx.clone());
        tokio::spawn(spawn_cache_poller(
            dragonfly.clone(),
            key.clone(),
            tx,
            Arc::clone(&latest),
            *interval_ms,
        ));
    }

    let cache = CacheBroadcasts {
        senders: Arc::new(senders),
        latest,
    };

    let state = AppState {
        dragonfly,
        order_events_tx,
        cache,
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
