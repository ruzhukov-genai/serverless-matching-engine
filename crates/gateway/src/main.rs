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
/// Values stored as Arc<str> — borrow() returns a refcount bump, zero allocation on read path.
pub(crate) struct CacheEntry {
    /// WS fan-out — subscribers get pushed new values (throttled to MAX_WS_BROADCASTS_PER_SEC)
    broadcast_tx: broadcast::Sender<Arc<str>>,
    /// Watch channel — REST handlers borrow() with zero allocation, no lock contention.
    watch_tx: tokio::sync::watch::Sender<Option<Arc<str>>>,
    watch_rx: tokio::sync::watch::Receiver<Option<Arc<str>>>,
    /// Last time this key was broadcast to WS clients — for rate limiting
    last_broadcast: std::sync::Mutex<std::time::Instant>,
}

/// Max WS broadcasts per key per second. Reduces fan-out from ~60/sec to 10/sec.
const WS_BROADCAST_INTERVAL_MS: u128 = 100;

/// Shared cache broadcasts — one poller per key, N subscribers.
/// REST handlers read via watch::borrow() (zero contention, zero allocation).
/// WS handlers subscribe to the broadcast channel.
#[derive(Clone)]
pub struct CacheBroadcasts {
    pub entries: Arc<HashMap<String, Arc<CacheEntry>>>,
}

impl CacheBroadcasts {
    /// Subscribe to updates for a cache key. Returns None if the key is unknown.
    pub fn subscribe(&self, key: &str) -> Option<broadcast::Receiver<Arc<str>>> {
        self.entries.get(key).map(|e| e.broadcast_tx.subscribe())
    }

    /// Get the last polled value for a cache key.
    /// Uses watch::borrow() — returns Arc<str> clone (refcount bump only, zero allocation).
    pub fn get_latest(&self, key: &str) -> Option<Arc<str>> {
        self.entries.get(key).and_then(|e| e.watch_rx.borrow().clone())
    }

    /// Update a cache key. Watch channel (REST) always gets latest.
    /// Broadcast channel (WS fan-out) is throttled to max 10/sec per key.
    fn update(&self, key: &str, val: String) {
        if let Some(e) = self.entries.get(key) {
            let arc_val: Arc<str> = Arc::from(val);
            // REST: always instant
            let _ = e.watch_tx.send(Some(arc_val.clone()));
            // WS: throttled — skip broadcast if <100ms since last
            if let Ok(mut last) = e.last_broadcast.lock() {
                if last.elapsed().as_millis() >= WS_BROADCAST_INTERVAL_MS {
                    let _ = e.broadcast_tx.send(arc_val);
                    *last = std::time::Instant::now();
                }
            }
        }
    }
}

/// Order dispatch mode — controls how gateway forwards orders to the worker.
#[derive(Clone, Debug, PartialEq)]
pub enum DispatchMode {
    /// LPUSH to Valkey queue (local dev / EC2 worker)
    Queue,
    /// Synchronous Lambda invoke — blocks until worker finishes (RequestResponse)
    Lambda,
    /// Async Lambda invoke — fire and forget, gateway returns 201 immediately (Event)
    LambdaAsync,
    /// SQS message — gateway sends to SQS, Lambda triggered by event source mapping
    Sqs,
}

#[derive(Clone)]
pub struct AppState {
    pub redis: RedisPool,
    /// Order events broadcast — single channel, WS clients filter by user_id
    pub order_events_tx: broadcast::Sender<String>,
    /// Shared cache broadcasts — one poller per key, N subscribers
    pub cache: CacheBroadcasts,
    /// Per-user TTL cache — avoids Valkey round-trips for orders/portfolio
    pub user_cache: routes::UserCache,
    /// Order dispatch mode: "queue" (default), "lambda", or "sqs"
    pub dispatch_mode: DispatchMode,
    /// AWS Lambda client — used when dispatch_mode == Lambda
    pub lambda_client: Option<aws_sdk_lambda::Client>,
    /// AWS SQS client — used when dispatch_mode == Sqs
    pub sqs_client: Option<aws_sdk_sqs::Client>,
    /// SQS queue URL — from ORDER_QUEUE_URL env var
    pub order_queue_url: String,
    /// Worker Lambda ARN — from WORKER_LAMBDA_ARN env var
    pub worker_lambda_arn: String,
}

/// Subscribe to Valkey pub/sub channel for cache updates.
/// Replaces 15 per-key polling tasks with a single subscriber.
/// Message format: "key\nvalue" (key on first line, rest is JSON value).
async fn spawn_cache_subscriber(
    redis_url: String,
    cache: CacheBroadcasts,
) {
    use deadpool_redis::redis::Client;
    use futures_util::StreamExt;

    loop {
        // Create a dedicated connection for SUBSCRIBE (can't use pool — blocks)
        let client = match Client::open(redis_url.as_str()) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "failed to create redis client for pub/sub");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };

        let mut pubsub = match client.get_async_pubsub().await {
            Ok(ps) => ps,
            Err(e) => {
                tracing::error!(error = %e, "failed to connect for pub/sub");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };

        if let Err(e) = pubsub.subscribe(sme_shared::cache::CACHE_UPDATES_CHANNEL).await {
            tracing::error!(error = %e, "failed to subscribe to cache_updates");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            continue;
        }

        tracing::info!("cache subscriber connected to pub/sub channel");
        let mut msg_stream = pubsub.on_message();

        loop {
            match msg_stream.next().await {
                Some(msg) => {
                    let payload: String = match msg.get_payload() {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    // Parse "key\nvalue" format
                    if let Some(newline_pos) = payload.find('\n') {
                        let key = &payload[..newline_pos];
                        let value = &payload[newline_pos + 1..];
                        if !value.is_empty() && value != "{}" {
                            cache.update(key, value.to_string());
                        }
                    }
                }
                None => {
                    tracing::warn!("pub/sub stream ended, reconnecting...");
                    break; // reconnect
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// Slow fallback poller — refreshes all cache keys every 30s.
/// Handles missed pub/sub messages (e.g. during reconnection).
async fn spawn_fallback_poller(
    pool: RedisPool,
    cache: CacheBroadcasts,
    keys: Vec<String>,
) {
    use deadpool_redis::redis::AsyncCommands;
    use tokio::time::{Duration, interval};

    let mut ticker = interval(Duration::from_secs(30));
    loop {
        ticker.tick().await;
        if let Ok(mut conn) = pool.get().await {
            for key in &keys {
                if let Ok(val) = conn.get::<_, String>(key).await {
                    if !val.is_empty() && val != "{}" {
                        cache.update(key, val);
                    }
                }
            }
        }
    }
}

// Default worker_threads = num_cpus (2 on this machine).
// Tested worker_threads=8: context-switch overhead on 2 vCPUs caused regression.
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    tracing::info!("sme-gateway starting");

    let config = sme_shared::Config::from_env();
    // Gateway needs far fewer connections than default (200):
    // - 15 cache pollers (1 each)
    // - Order LPUSH burst (~5 concurrent)
    // - Per-user fallback reads (declining with TTL cache)
    // Gateway needs fewer connections than default (200):
    // - 15 cache pollers (transient checkout per interval)
    // - Order LPUSH burst (~10 concurrent at peak)
    // - Per-user fallback reads (declining with TTL cache)
    let redis = sme_shared::cache::create_pool_sized(&config.redis_url, 30).await?;

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
        let (broadcast_tx, _) = broadcast::channel::<Arc<str>>(64);
        let (watch_tx, watch_rx) = tokio::sync::watch::channel::<Option<Arc<str>>>(None);
        entries.insert(key.clone(), Arc::new(CacheEntry {
            broadcast_tx,
            watch_tx,
            watch_rx,
            last_broadcast: std::sync::Mutex::new(std::time::Instant::now()),
        }));
    }

    let cache = CacheBroadcasts {
        entries: Arc::new(entries),
    };

    // Warm cache: one-time GET for all keys (pub/sub only delivers new messages)
    {
        use deadpool_redis::redis::AsyncCommands;
        if let Ok(mut conn) = redis.get().await {
            for (key, _) in &poll_specs {
                if let Ok(val) = conn.get::<_, String>(key).await {
                    if !val.is_empty() && val != "{}" {
                        cache.update(key, val);
                    }
                }
            }
        }
        tracing::info!(keys = poll_specs.len(), "cache warmed from Valkey");
    }

    // Single pub/sub subscriber replaces 15 per-key pollers
    let all_keys: Vec<String> = poll_specs.iter().map(|(k, _)| k.clone()).collect();
    tokio::spawn(spawn_cache_subscriber(
        config.redis_url.clone(),
        cache.clone(),
    ));

    // Slow fallback poller (every 30s) — handles missed messages during reconnection
    tokio::spawn(spawn_fallback_poller(
        redis.clone(),
        cache.clone(),
        all_keys,
    ));

    // Order dispatch mode — queue (local dev), lambda, or sqs (AWS production)
    let dispatch_mode = match std::env::var("ORDER_DISPATCH_MODE").as_deref() {
        Ok("lambda") => {
            tracing::info!("order dispatch mode: lambda (sync RequestResponse)");
            DispatchMode::Lambda
        }
        Ok("lambda-async") => {
            tracing::info!("order dispatch mode: lambda-async (async Event, fire-and-forget)");
            DispatchMode::LambdaAsync
        }
        Ok("sqs") => {
            tracing::info!("order dispatch mode: sqs");
            DispatchMode::Sqs
        }
        _ => {
            tracing::info!("order dispatch mode: queue (default, Valkey LPUSH)");
            DispatchMode::Queue
        }
    };

    let worker_lambda_arn = std::env::var("WORKER_LAMBDA_ARN").unwrap_or_default();
    let order_queue_url = std::env::var("ORDER_QUEUE_URL").unwrap_or_default();

    let aws_cfg = if dispatch_mode == DispatchMode::Lambda || dispatch_mode == DispatchMode::LambdaAsync || dispatch_mode == DispatchMode::Sqs {
        Some(aws_config::load_from_env().await)
    } else {
        None
    };

    let lambda_client = if dispatch_mode == DispatchMode::Lambda || dispatch_mode == DispatchMode::LambdaAsync {
        Some(aws_sdk_lambda::Client::new(aws_cfg.as_ref().unwrap()))
    } else {
        None
    };

    let sqs_client = if dispatch_mode == DispatchMode::Sqs {
        Some(aws_sdk_sqs::Client::new(aws_cfg.as_ref().unwrap()))
    } else {
        None
    };

    let state = AppState {
        redis: redis.clone(),
        order_events_tx,
        cache: cache.clone(),
        user_cache: routes::UserCache::new(),
        dispatch_mode,
        lambda_client,
        sqs_client,
        order_queue_url,
        worker_lambda_arn,
    };

    let app = Router::new()
        // Trading API - all read from Valkey cache
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
        // Batch API — one request replaces 8 REST polls
        .route("/api/snapshot/{pair_id}", get(routes::get_snapshot))
        // SSE — single connection pushes all updates
        .route("/api/stream/{pair_id}", get(routes::sse_stream))
        // WebSocket feeds
        .route("/ws/stream", get(routes::ws_stream))
        .route("/ws/orderbook/{pair_id}", get(routes::ws_orderbook))
        .route("/ws/trades/{pair_id}", get(routes::ws_trades))
        .route("/ws/orders/{user_id}", get(routes::ws_orders))
        // Dashboard API - all read from Valkey cache
        .route("/api/metrics", get(routes::get_metrics))
        .route("/api/metrics/locks", get(routes::get_lock_metrics))
        .route("/api/metrics/throughput", get(routes::get_throughput))
        .route("/api/metrics/latency", get(routes::get_latency_percentiles))
        .route("/api/metrics/audit", get(routes::get_audit))
        .route("/api/audit", get(routes::get_audit))
        // Serve static files
        .nest_service("/trading", ServeDir::new("web/trading"))
        .nest_service("/dashboard", ServeDir::new("web/dashboard"))
        // CompressionLayer removed: on 2-vCPU machine, gzip CPU cost exceeds
        // bandwidth savings for small cached JSON responses. Net regression.
        .layer(CorsLayer::permissive())
        .with_state(state);

    // ── Gateway instrumentation — log request stats every 30s ───────────────
    let request_count = Arc::new(AtomicU64::new(0));
    let ws_connections = Arc::new(AtomicU64::new(0));
    {
        let req_count = request_count.clone();
        let ws_conns = ws_connections.clone();
        let cache_ref = cache.clone();
        let pool_ref = redis.clone();
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
    tracing::info!("listening on http://{} (h2c + TCP_NODELAY)", addr);

    // Use hyper directly for HTTP/1.1 + h2c (cleartext HTTP/2) auto-detection.
    loop {
        let (stream, _remote) = listener.accept().await?;
        // TCP_NODELAY: disable Nagle's algorithm — send small frames immediately.
        // Critical for low-latency JSON responses (typically <4KB).
        let _ = stream.set_nodelay(true);
        let app = app.clone();
        tokio::spawn(async move {
            let hyper_svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let app = app.clone();
                async move {
                    let (parts, body) = req.into_parts();
                    let body = axum::body::Body::new(body);
                    let req = hyper::Request::from_parts(parts, body);
                    let resp = tower_service::Service::call(&mut app.clone(), req).await;
                    resp.map(|r| {
                        let (parts, body) = r.into_parts();
                        hyper::Response::from_parts(parts, body)
                    })
                }
            });
            let builder = hyper_util::server::conn::auto::Builder::new(
                hyper_util::rt::TokioExecutor::new(),
            );
            if let Err(e) = builder.serve_connection_with_upgrades(
                hyper_util::rt::TokioIo::new(stream),
                hyper_svc,
            ).await {
                if !e.to_string().contains("connection reset") {
                    tracing::debug!(error = %e, "connection ended");
                }
            }
        });
    }
}
