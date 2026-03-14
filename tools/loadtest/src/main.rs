//! Load test for the Serverless Matching Engine API.
//!
//! Measures orders/sec and matches/sec under increasing concurrency.
//!
//! Usage:
//!   cargo run --release -- [OPTIONS]
//!
//! Options:
//!   --url <BASE_URL>         API base URL (default: http://localhost:3001)
//!   --pairs <PAIRS>          Comma-separated pair IDs (default: BTC-USDT)
//!   --concurrency <LEVELS>   Comma-separated concurrency levels (default: 1,2,4,8,16,32)
//!   --duration <SECS>        Seconds per concurrency level (default: 10)
//!   --scenario <NAME>        Test scenario (default: all)
//!
//! Scenarios:
//!   resting    — place non-crossing limit orders (no matching, pure order flow)
//!   crossing   — place crossing orders that always match (order + match flow)
//!   mixed      — 50% resting, 50% crossing
//!   multi-pair — crossing orders across multiple pairs
//!   all        — run all scenarios sequentially

use anyhow::Result;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Barrier;

// ── Config ────────────────────────────────────────────────────────────────────

struct Config {
    base_url: String,
    pairs: Vec<String>,
    concurrency_levels: Vec<usize>,
    duration_secs: u64,
    scenarios: Vec<String>,
}

impl Config {
    fn from_args() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let get = |flag: &str, default: &str| -> String {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1))
                .cloned()
                .unwrap_or_else(|| default.to_string())
        };

        let pairs: Vec<String> = get("--pairs", "BTC-USDT")
            .split(',')
            .map(|s| s.trim().to_string())
            .collect();

        let concurrency_levels: Vec<usize> = get("--concurrency", "1,2,4,8,16,32")
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();

        let scenario = get("--scenario", "all");
        let scenarios = if scenario == "all" {
            vec![
                "resting".to_string(),
                "crossing".to_string(),
                "mixed".to_string(),
                "multi-pair".to_string(),
            ]
        } else {
            vec![scenario]
        };

        Config {
            base_url: get("--url", "http://localhost:3001"),
            pairs,
            concurrency_levels,
            duration_secs: get("--duration", "10").parse().unwrap_or(10),
            scenarios,
        }
    }
}

// ── Stats ─────────────────────────────────────────────────────────────────────

struct Stats {
    orders: AtomicU64,
    trades: AtomicU64,
    errors: AtomicU64,
    latency_sum_us: AtomicU64,
    latency_max_us: AtomicU64,
}

impl Stats {
    fn new() -> Arc<Self> {
        Arc::new(Stats {
            orders: AtomicU64::new(0),
            trades: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            latency_sum_us: AtomicU64::new(0),
            latency_max_us: AtomicU64::new(0),
        })
    }

    fn record(&self, trades: u64, latency: Duration) {
        self.orders.fetch_add(1, Ordering::Relaxed);
        self.trades.fetch_add(trades, Ordering::Relaxed);
        let us = latency.as_micros() as u64;
        self.latency_sum_us.fetch_add(us, Ordering::Relaxed);
        self.latency_max_us.fetch_max(us, Ordering::Relaxed);
    }

    fn record_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> (u64, u64, u64, u64, u64) {
        (
            self.orders.load(Ordering::Relaxed),
            self.trades.load(Ordering::Relaxed),
            self.errors.load(Ordering::Relaxed),
            self.latency_sum_us.load(Ordering::Relaxed),
            self.latency_max_us.load(Ordering::Relaxed),
        )
    }
}

// ── Order Generators ──────────────────────────────────────────────────────────

/// Non-crossing limit orders: alternate buy@70500 and sell@71000 (no match)
fn resting_order(pair: &str, seq: u64) -> Value {
    let (side, price) = if seq.is_multiple_of(2) {
        ("Buy", "70500")
    } else {
        ("Sell", "71000")
    };
    let user = if seq.is_multiple_of(2) { "user-1" } else { "user-2" };
    json!({
        "user_id": user,
        "pair_id": pair,
        "side": side,
        "order_type": "Limit",
        "tif": "IOC",
        "price": price,
        "quantity": "0.00001"
    })
}

/// Crossing orders: even = GTC sell that rests, odd = GTC buy that matches
fn crossing_order(pair: &str, seq: u64) -> Value {
    let (side, price, user) = if seq.is_multiple_of(2) {
        ("Sell", "70735", "user-2")
    } else {
        ("Buy", "70735", "user-1")
    };
    json!({
        "user_id": user,
        "pair_id": pair,
        "side": side,
        "order_type": "Limit",
        "tif": "GTC",
        "price": price,
        "quantity": "0.00001"
    })
}

/// Mixed: 50% resting, 50% crossing
fn mixed_order(pair: &str, seq: u64) -> Value {
    if seq % 4 < 2 {
        resting_order(pair, seq)
    } else {
        crossing_order(pair, seq)
    }
}

// ── API Helpers ───────────────────────────────────────────────────────────────

async fn reset_db(client: &Client, base_url: &str) -> Result<()> {
    // Place a tiny order to trigger DB init, then we rely on IOC orders
    // that don't accumulate state
    let _ = client
        .get(format!("{base_url}/api/pairs"))
        .send()
        .await?;
    Ok(())
}

async fn place_order(client: &Client, base_url: &str, body: &Value) -> Result<u64> {
    let resp = client
        .post(format!("{base_url}/api/orders"))
        .json(body)
        .send()
        .await?;

    if !resp.status().is_success() && resp.status().as_u16() != 200 {
        anyhow::bail!("HTTP {}", resp.status());
    }

    let json: Value = resp.json().await?;
    let trades = json
        .get("trades")
        .and_then(|t| t.as_array())
        .map(|a| a.len() as u64)
        .unwrap_or(0);

    Ok(trades)
}

// ── Scenario Runner ───────────────────────────────────────────────────────────

async fn run_scenario(
    name: &str,
    config: &Config,
    client: &Client,
    order_fn: fn(&str, u64) -> Value,
) -> Result<()> {
    println!("\n{}", "=".repeat(60));
    println!("  Scenario: {name}");
    println!("  Duration: {}s per level", config.duration_secs);
    println!("  Pairs: {:?}", config.pairs);
    println!("{}", "=".repeat(60));
    println!(
        "{:>6} {:>10} {:>10} {:>10} {:>10} {:>10} {:>8}",
        "conc", "orders", "ord/sec", "trades", "trd/sec", "avg_ms", "max_ms"
    );
    println!("{}", "-".repeat(72));

    for &conc in &config.concurrency_levels {
        let stats = Stats::new();
        let barrier = Arc::new(Barrier::new(conc + 1)); // +1 for main thread
        let deadline = Instant::now() + Duration::from_secs(config.duration_secs);

        let mut handles = Vec::new();

        for worker in 0..conc {
            let client = client.clone();
            let stats = stats.clone();
            let barrier = barrier.clone();
            let base_url = config.base_url.clone();
            let pair = config.pairs[worker % config.pairs.len()].clone();
            let seq_start = (worker as u64) * 1_000_000;

            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                let mut seq = seq_start;
                while Instant::now() < deadline {
                    let body = order_fn(&pair, seq);
                    let start = Instant::now();
                    match place_order(&client, &base_url, &body).await {
                        Ok(trades) => stats.record(trades, start.elapsed()),
                        Err(_) => stats.record_error(),
                    }
                    seq += 1;
                }
            }));
        }

        // Release all workers simultaneously
        barrier.wait().await;
        let wall_start = Instant::now();

        for h in handles {
            let _ = h.await;
        }
        let wall_elapsed = wall_start.elapsed().as_secs_f64();

        let (orders, trades, errors, lat_sum, lat_max) = stats.snapshot();
        let ord_sec = orders as f64 / wall_elapsed;
        let trd_sec = trades as f64 / wall_elapsed;
        let avg_ms = if orders > 0 {
            (lat_sum as f64 / orders as f64) / 1000.0
        } else {
            0.0
        };
        let max_ms = lat_max as f64 / 1000.0;

        println!(
            "{:>6} {:>10} {:>10.1} {:>10} {:>10.1} {:>10.2} {:>8.1}{}",
            conc,
            orders,
            ord_sec,
            trades,
            trd_sec,
            avg_ms,
            max_ms,
            if errors > 0 {
                format!("  ({errors} err)")
            } else {
                String::new()
            }
        );
    }

    Ok(())
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_args();

    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║         SME Load Test — Orders & Matches/sec           ║");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║  URL: {:<51}║", config.base_url);
    println!("║  Concurrency: {:?}{:>width$}║",
        config.concurrency_levels,
        "",
        width = 43 - format!("{:?}", config.concurrency_levels).len()
    );
    println!("║  Duration: {}s per level{:>width$}║",
        config.duration_secs,
        "",
        width = 34 - config.duration_secs.to_string().len()
    );
    println!("╚══════════════════════════════════════════════════════════╝");

    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(64)
        .build()?;

    reset_db(&client, &config.base_url).await?;

    for scenario in &config.scenarios {
        match scenario.as_str() {
            "resting" => {
                run_scenario(
                    "Resting Orders (no matching)",
                    &config,
                    &client,
                    resting_order,
                )
                .await?;
            }
            "crossing" => {
                run_scenario(
                    "Crossing Orders (every 2nd matches)",
                    &config,
                    &client,
                    crossing_order,
                )
                .await?;
            }
            "mixed" => {
                run_scenario(
                    "Mixed (50% resting, 50% crossing)",
                    &config,
                    &client,
                    mixed_order,
                )
                .await?;
            }
            "multi-pair" => {
                if config.pairs.len() < 2 {
                    println!("\n  [skip multi-pair: need --pairs BTC-USDT,ETH-USDT,SOL-USDT]");
                    continue;
                }
                run_scenario(
                    "Multi-Pair Crossing",
                    &config,
                    &client,
                    crossing_order,
                )
                .await?;
            }
            _ => {
                println!("Unknown scenario: {scenario}");
            }
        }
    }

    println!("\n✅ Load test complete.");
    Ok(())
}
