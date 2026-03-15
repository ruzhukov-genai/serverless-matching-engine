use anyhow::Result;
use deadpool_redis::Pool as RedisPool;
use deadpool_redis::redis::AsyncCommands;
use rust_decimal::Decimal;
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tracing_subscriber::EnvFilter;
use serde_json::{json, Value};

mod routes;
mod worker;

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
    /// Hot path pool — used by order writes.
    pub pg: PgPool,
    /// Background pool — async persist + read-only queries.
    pub pg_bg: PgPool,
    /// In-memory pairs cache: pair_id → PairConfig
    pub pairs_cache: Arc<HashMap<String, PairConfig>>,
    /// Channel to the background persistence worker
    pub persist_tx: mpsc::Sender<routes::PersistJob>,
    /// Order events broadcast — all order state changes pushed here
    pub order_events_tx: broadcast::Sender<String>,
    /// Dirty user IDs — notifies cache refresh worker which portfolios to update
    pub dirty_users_tx: mpsc::Sender<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    tracing::info!("sme-api (worker) starting");

    let config = sme_shared::Config::from_env();
    let dragonfly = sme_shared::cache::create_pool(&config.dragonfly_url).await?;

    // Hot path pool: low latency, fast order inserts + balance locks.
    let pg_hot = sqlx::postgres::PgPoolOptions::new()
        .max_connections(60)
        .min_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .idle_timeout(Duration::from_secs(120))
        .max_lifetime(Duration::from_secs(1800))
        .connect(&config.database_url)
        .await
        .map_err(|e| anyhow::anyhow!("connect pg_hot: {}", e))?;

    // Background pool: async persist + read-only queries (portfolio, orders, pairs).
    let pg_bg = sqlx::postgres::PgPoolOptions::new()
        .max_connections(40)
        .min_connections(3)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(120))
        .max_lifetime(Duration::from_secs(1800))
        .connect(&config.database_url)
        .await
        .map_err(|e| anyhow::anyhow!("connect pg_bg: {}", e))?;

    tracing::info!("running migrations");
    sme_shared::db::run_migrations(&pg_hot).await?;

    // Run seed SQL directly
    tracing::info!("seeding initial data");
    run_seed(&pg_hot).await?;

    // Load pairs into memory cache — eliminates SELECT on every order validation
    tracing::info!("loading pairs cache");
    let pairs_cache = Arc::new(load_pairs_cache(&pg_hot).await?);
    tracing::info!(count = pairs_cache.len(), "pairs cache loaded");

    // Initialize cache keys in Dragonfly
    tracing::info!("initializing cache keys");
    initialize_cache_keys(&dragonfly, &pg_hot, &pairs_cache).await?;

    // Spawn background persistence worker — uses dedicated pg_bg pool
    let (persist_tx, persist_rx) = mpsc::channel::<routes::PersistJob>(1000);
    routes::spawn_persist_worker(pg_bg.clone(), persist_rx);

    // Order events broadcast — single channel, gateway WS clients filter by user_id
    let (order_events_tx, _) = broadcast::channel::<String>(1024);

    // Dirty user channel — order workers notify cache refresh which portfolios changed
    let (dirty_users_tx, dirty_users_rx) = mpsc::channel::<String>(10_000);

    let state = AppState {
        dragonfly: dragonfly.clone(), pg: pg_hot, pg_bg: pg_bg.clone(), pairs_cache,
        persist_tx, order_events_tx, dirty_users_tx,
    };

    // Start the order queue consumer
    let worker_state = state.clone();
    tokio::spawn(async move {
        worker::order_queue_consumer(worker_state).await;
    });

    // Start cache refresh workers
    let cache_state = state.clone();
    tokio::spawn(async move {
        worker::cache_refresh_worker(cache_state, dirty_users_rx).await;
    });

    // Background metrics refresh (same as before)
    let metrics_dragonfly = dragonfly.clone();
    let metrics_pg = pg_bg.clone();
    tokio::spawn(async move {
        routes::metrics_refresh_loop_worker(metrics_dragonfly, metrics_pg).await;
    });

    // Background ticker + trades refresh (same as before)
    let ticker_dragonfly = dragonfly.clone();
    let ticker_pg = pg_bg.clone();
    tokio::spawn(async move {
        routes::ticker_trades_refresh_loop_worker(ticker_dragonfly, ticker_pg).await;
    });

    // Background portfolio refresh (same as before)
    let portfolio_dragonfly = dragonfly.clone();
    let portfolio_pg = pg_bg.clone();
    tokio::spawn(async move {
        routes::portfolio_refresh_loop_worker(portfolio_dragonfly, portfolio_pg).await;
    });

    tracing::info!("worker started, listening for orders on queue:orders");

    // Keep the worker alive
    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down worker");

    Ok(())
}

async fn load_pairs_cache(pg: &PgPool) -> Result<HashMap<String, PairConfig>> {
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

    // Balances — 10 users with deep pockets for load testing
    for user_num in 1..=10 {
        let user = format!("user-{user_num}");
        for (asset, avail) in [
            ("BTC", "10000"),
            ("ETH", "100000"),
            ("SOL", "1000000"),
            ("USDT", "1000000000"),  // 1B USDT
        ] {
            let avail_d = Decimal::from_str(avail).unwrap();
            sqlx::query(
                "INSERT INTO balances (user_id, asset, available, locked) VALUES ($1,$2,$3,0)
                 ON CONFLICT (user_id, asset) DO UPDATE SET available = GREATEST(balances.available, $3)",
            )
            .bind(&user).bind(asset).bind(avail_d)
            .execute(pg).await?;
        }
    }

    Ok(())
}

async fn initialize_cache_keys(
    dragonfly: &RedisPool,
    pg: &PgPool,
    pairs_cache: &HashMap<String, PairConfig>,
) -> Result<()> {
    let mut conn = dragonfly.get().await?;

    // Initialize cache:pairs
    let rows = sqlx::query("SELECT id, base, quote, tick_size, lot_size, min_order_size, max_order_size, price_precision, qty_precision, price_band_pct, active FROM pairs WHERE active = true")
        .fetch_all(pg)
        .await?;

    let pairs: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<String, _>("id"),
                "base": r.get::<String, _>("base"),
                "quote": r.get::<String, _>("quote"),
                "tick_size": r.get::<Decimal, _>("tick_size").to_string(),
                "lot_size": r.get::<Decimal, _>("lot_size").to_string(),
                "min_order_size": r.get::<Decimal, _>("min_order_size").to_string(),
                "max_order_size": r.get::<Decimal, _>("max_order_size").to_string(),
                "price_precision": r.get::<i16, _>("price_precision"),
                "qty_precision": r.get::<i16, _>("qty_precision"),
                "price_band_pct": r.get::<Decimal, _>("price_band_pct").to_string(),
                "active": r.get::<bool, _>("active"),
            })
        })
        .collect();

    let pairs_json = json!({"pairs": pairs});
    let pairs_str = serde_json::to_string(&pairs_json)?;
    conn.set::<_, _, ()>("cache:pairs", &pairs_str).await?;

    // Initialize empty caches for each pair
    for pair_id in pairs_cache.keys() {
        // Empty orderbook
        let empty_book = json!({
            "pair": pair_id,
            "bids": [],
            "asks": []
        });
        let book_str = serde_json::to_string(&empty_book)?;
        conn.set::<_, _, ()>(format!("cache:orderbook:{}", pair_id), &book_str).await?;

        // Empty ticker
        let empty_ticker = json!({
            "pair": pair_id,
            "last": null,
            "high_24h": null,
            "low_24h": null,
            "volume_24h": null
        });
        let ticker_str = serde_json::to_string(&empty_ticker)?;
        conn.set::<_, _, ()>(format!("cache:ticker:{}", pair_id), &ticker_str).await?;

        // Empty trades
        let empty_trades = json!({
            "pair": pair_id,
            "trades": []
        });
        let trades_str = serde_json::to_string(&empty_trades)?;
        conn.set::<_, _, ()>(format!("cache:trades:{}", pair_id), &trades_str).await?;
    }

    // Empty metrics caches
    conn.set::<_, _, ()>("cache:metrics", "{}").await?;
    conn.set::<_, _, ()>("cache:lock_metrics", "{}").await?;
    conn.set::<_, _, ()>("cache:throughput", "{\"series\": []}").await?;
    conn.set::<_, _, ()>("cache:latency_metrics", "{}").await?;
    conn.set::<_, _, ()>("cache:audit", "{\"audit\": [], \"events\": []}").await?;

    tracing::info!("cache keys initialized");
    Ok(())
}