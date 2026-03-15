use anyhow::Result;
use axum::{Router, routing::get};
use deadpool_redis::Pool as RedisPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

mod routes;

/// Per-key cache entry: watch channel for zero-contention reads + broadcast for WS fan-out.
struct CacheEntry {
    /// WS fan-out — subscribers get pushed new values
    broadcast_tx: broadcast::Sender<String>,
    /// Watch channel — REST handlers borrow() with zero allocation, no lock contention.
    /// Replaces the old RwLock<HashMap> which serialized all reads behind writer locks.
    watch_tx: tokio::sync::watch::Sender<Option<String>>,
    watch_rx: tokio::sync::watch::Receiver<Option<String>>,
}

/// Shared cache broadcasts — one poller per key, N subscribers.
/// REST handlers read via watch::borrow() (zero contention, zero allocation).
/// WS handlers subscribe to the broadcast channel.
#[derive(Clone)]
pub struct CacheBroadcasts {
    entries: Arc<HashMap<String, Arc<CacheEntry>>>,
}

impl CacheBroadcasts {
    /// Subscribe to updates for a cache key. Returns None if the key is unknown.
    pub fn subscribe(&self, key: &str) -> Option<broadcast::Receiver<String>> {
        self.entries.get(key).map(|e| e.broadcast_tx.subscribe())
    }

    /// Get the last polled value for a cache key.
    /// Uses watch::borrow() — no async, no lock, no allocation beyond the clone.
    pub async fn get_latest(&self, key: &str) -> Option<String> {
        self.entries.get(key).and_then(|e| e.watch_rx.borrow().clone())
    }

    /// Update a cache key (called by pollers). Sends to both watch + broadcast.
    fn update(&self, key: &str, val: String) {
        if let Some(e) = self.entries.get(key) {
            let _ = e.watch_tx.send(Some(val.clone()));
            let _ = e.broadcast_tx.send(val);
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub dragonfly: RedisPool,
    /// Order events broadcast — single channel, WS clients filter by user_id
    pub order_events_tx: broadcast::Sender<String>,
    /// Shared cache broadcasts — one poller per key, N subscribers
    pub cache: CacheBroadcasts,
    /// Per-user TTL cache — avoids Dragonfly round-trips for orders/portfolio
    pub user_cache: routes::UserCache,
}

/// Background poller: reads one key from Dragonfly at `interval_ms`,
/// updates the CacheBroadcasts entry (watch + broadcast channels).
async fn spawn_cache_poller(
    pool: RedisPool,
    key: String,
    cache: CacheBroadcasts,
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
                    cache.update(&key, val);
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

    let mut entries: HashMap<String, Arc<CacheEntry>> = HashMap::new();

    for (key, _interval_ms) in &poll_specs {
        let (broadcast_tx, _) = broadcast::channel::<String>(64);
        let (watch_tx, watch_rx) = tokio::sync::watch::channel(None);
        entries.insert(key.clone(), Arc::new(CacheEntry {
            broadcast_tx,
            watch_tx,
            watch_rx,
        }));
    }

    let cache = CacheBroadcasts {
        entries: Arc::new(entries),
    };

    for (key, interval_ms) in &poll_specs {
        tokio::spawn(spawn_cache_poller(
            dragonfly.clone(),
            key.clone(),
            cache.clone(),
            *interval_ms,
        ));
    }

    let state = AppState {
        dragonfly: dragonfly.clone(),
        order_events_tx,
        cache: cache.clone(),
        user_cache: routes::UserCache::new(),
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

    // ── Gateway instrumentation — log request stats every 30s ───────────────
    let request_count = Arc::new(AtomicU64::new(0));
    let ws_connections = Arc::new(AtomicU64::new(0));
    {
        let req_count = request_count.clone();
        let ws_conns = ws_connections.clone();
        let cache_ref = cache.clone();
        let pool_ref = dragonfly.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                ticker.tick().await;
                let reqs = req_count.swap(0, Ordering::Relaxed);
                let ws = ws_conns.load(Ordering::Relaxed);
                let pool_status = pool_ref.status();
                let cache_keys = cache_ref.entries.len();
                tracing::info!(
                    requests_30s = reqs,
                    ws_connections = ws,
                    df_pool_size = pool_status.size,
                    df_pool_available = pool_status.available,
                    cache_keys = cache_keys,
                    "[gateway-stats]"
                );
            }
        });
    }

    let port = std::env::var("PORT").unwrap_or_else(|_| "3001".to_string());
    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on http://{}", addr);
    axum::serve(listener, app).await?;

    Ok(())
}
