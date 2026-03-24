//! Load test for the Serverless Matching Engine API.
//!
//! Measures dispatch/sec (Gateway HTTP throughput) and optionally verified
//! trades/sec via the multiplexed WebSocket (/ws/stream).
//!
//! Usage: cargo run --release -- [OPTIONS]
//!
//! Options:
//!   --url <BASE_URL>         API base URL (default: http://localhost:3001)
//!   --pairs <PAIRS>          Comma-separated pair IDs (default: BTC-USDT)
//!   --concurrency <LEVELS>   Comma-separated concurrency levels (default: 1,2,4,8,16,32)
//!   --duration <SECS>        Seconds per concurrency level (default: 10)
//!   --scenario <NAME>        Test scenario (default: all)
//!   --mode <MODE>            sync (default) or verified
//!   --label <LABEL>          Environment label (e.g. local-docker, aws-lambda)
//!   --output <FILE>          Write JSON results to file
//!
//! Scenarios: resting | crossing | mixed | multi-pair | all
//! Modes:
//!   sync     -- fire-and-forget, measures raw Gateway dispatch throughput
//!   verified -- opens WebSocket to /ws/stream, counts confirmed trade events

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Barrier;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

// -- Config -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Sync,
    Verified,
}

struct Config {
    base_url: String,
    ws_url: Option<String>,
    pairs: Vec<String>,
    concurrency_levels: Vec<usize>,
    duration_secs: u64,
    scenarios: Vec<String>,
    mode: Mode,
    label: String,
    output: Option<String>,
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
            .split(",")
            .map(|s| s.trim().to_string())
            .collect();

        let concurrency_levels: Vec<usize> = get("--concurrency", "1,2,4,8,16,32")
            .split(",")
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

        let mode = match get("--mode", "sync").as_str() {
            "verified" => Mode::Verified,
            _ => Mode::Sync,
        };

        let label = get("--label", "unknown");
        let output = args
            .iter()
            .position(|a| a == "--output")
            .and_then(|i| args.get(i + 1))
            .cloned();

        let ws_url = args
            .iter()
            .position(|a| a == "--ws-url")
            .and_then(|i| args.get(i + 1))
            .cloned();

        Config {
            base_url: get("--url", "http://localhost:3001"),
            ws_url,
            pairs,
            concurrency_levels,
            duration_secs: get("--duration", "10").parse().unwrap_or(10),
            scenarios,
            mode,
            label,
            output,
        }
    }

    fn ws_base_url(&self) -> String {
        if let Some(ref url) = self.ws_url {
            return url.clone();
        }
        self.base_url
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1)
    }
}

// -- Stats --------------------------------------------------------------------

struct Stats {
    orders: AtomicU64,
    errors: AtomicU64,
    latency_sum_us: AtomicU64,
    latency_max_us: AtomicU64,
}

impl Stats {
    fn new() -> Arc<Self> {
        Arc::new(Stats {
            orders: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            latency_sum_us: AtomicU64::new(0),
            latency_max_us: AtomicU64::new(0),
        })
    }

    fn record(&self, latency: Duration) {
        self.orders.fetch_add(1, Ordering::Relaxed);
        let us = latency.as_micros() as u64;
        self.latency_sum_us.fetch_add(us, Ordering::Relaxed);
        self.latency_max_us.fetch_max(us, Ordering::Relaxed);
    }

    fn record_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.orders.load(Ordering::Relaxed),
            self.errors.load(Ordering::Relaxed),
            self.latency_sum_us.load(Ordering::Relaxed),
            self.latency_max_us.load(Ordering::Relaxed),
        )
    }
}

// -- Order Generators ---------------------------------------------------------

fn resting_order(pair: &str, seq: u64) -> Value {
    let (side, price) = if seq % 2 == 0 {
        ("Buy", "70500")
    } else {
        ("Sell", "71000")
    };
    let user = if seq % 2 == 0 { "user-1" } else { "user-2" };
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

fn crossing_order(pair: &str, seq: u64) -> Value {
    let (side, price, user) = if seq % 2 == 0 {
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

fn mixed_order(pair: &str, seq: u64) -> Value {
    if seq % 4 < 2 {
        resting_order(pair, seq)
    } else {
        crossing_order(pair, seq)
    }
}

// -- API Helpers --------------------------------------------------------------

async fn warm_up(client: &Client, base_url: &str) -> Result<()> {
    let _ = client.get(format!("{base_url}/api/pairs")).send().await?;
    Ok(())
}

async fn place_order(client: &Client, base_url: &str, body: &Value) -> Result<()> {
    let resp = client
        .post(format!("{base_url}/api/orders"))
        .json(body)
        .send()
        .await?;
    let status = resp.status();
    let _ = resp.bytes().await;
    if !status.is_success() {
        anyhow::bail!("HTTP {}", status);
    }
    Ok(())
}

// -- WebSocket Trade Counter --------------------------------------------------

async fn start_ws_trade_counter(
    ws_url: String,
    pairs: Vec<String>,
) -> Result<(Arc<AtomicU64>, tokio::sync::oneshot::Sender<()>)> {
    let trade_count = Arc::new(AtomicU64::new(0));
    let trade_count_clone = trade_count.clone();
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        if let Err(e) = ws_trade_loop(ws_url, pairs, trade_count_clone, cancel_rx).await {
            eprintln!("[ws] trade counter error: {e}");
        }
    });

    // Allow WS connection to establish before sending orders
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok((trade_count, cancel_tx))
}

async fn ws_trade_loop(
    ws_url: String,
    pairs: Vec<String>,
    trade_count: Arc<AtomicU64>,
    mut cancel_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    // On AWS: ws_url is wss://xxx.execute-api.../ws (complete, no suffix needed)
    // Locally: ws_url is ws://localhost:3001 (needs /ws/stream suffix)
    let url = if ws_url.contains("execute-api") || ws_url.ends_with("/ws") {
        ws_url.clone()
    } else {
        format!("{ws_url}/ws/stream")
    };
    let (ws_stream, _) = connect_async(&url).await?;
    let (mut write, mut read) = ws_stream.split();

    let channels: Vec<String> = pairs.iter().map(|p| format!("trades:{p}")).collect();
    write
        .send(WsMessage::Text(
            json!({ "subscribe": channels }).to_string().into(),
        ))
        .await?;

    loop {
        tokio::select! {
            _ = &mut cancel_rx => {
                let _ = write.send(WsMessage::Close(None)).await;
                break;
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        // {"ch":"trades:BTC-USDT","data":{...}}
                        if let Ok(val) = serde_json::from_str::<Value>(&text) {
                            let is_trade = val
                                .get("ch")
                                .and_then(|c| c.as_str())
                                .map(|ch| ch.starts_with("trades:"))
                                .unwrap_or(false);
                            if is_trade {
                                if let Some(data) = val.get("data") {
                                    let n = if data.is_array() {
                                        data.as_array().map(|a| a.len() as u64).unwrap_or(1)
                                    } else {
                                        1
                                    };
                                    trade_count.fetch_add(n, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = write.send(WsMessage::Pong(data)).await;
                    }
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

// -- Result Types -------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
struct LevelResult {
    concurrency: usize,
    orders: u64,
    errors: u64,
    dispatch_per_sec: f64,
    avg_latency_ms: f64,
    max_latency_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    verified_trades: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verified_trades_per_sec: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    match_rate_pct: Option<f64>,
}

#[derive(Debug, serde::Serialize)]
struct ScenarioResult {
    label: String,
    scenario: String,
    mode: String,
    levels: Vec<LevelResult>,
}

// -- Summary Box --------------------------------------------------------------

fn print_summary(label: &str, scenario: &str, result: &LevelResult) {
    let w = 57usize;
    let top = format!("╔{}╗", "═".repeat(w));
    let bot = format!("╚{}╝", "═".repeat(w));
    let row = |s: String| {
        let content = w - 2;
        let text = format!("  {s}");
        let pad = content.saturating_sub(text.len());
        format!("║{text}{:>pad$}║", "", pad = pad)
    };
    println!("{top}");
    println!("{}", row(format!("Environment: {label}")));
    println!(
        "{}",
        row(format!(
            "Scenario: {scenario} @ concurrency={}",
            result.concurrency
        ))
    );
    println!(
        "{}",
        row(format!(
            "Dispatch: {:.1} orders/sec ({:.0}ms avg)",
            result.dispatch_per_sec, result.avg_latency_ms
        ))
    );
    if let (Some(tps), Some(mr)) = (result.verified_trades_per_sec, result.match_rate_pct) {
        println!("{}", row(format!("Verified trades: {:.1} trades/sec", tps)));
        println!("{}", row(format!("Match rate: {:.1}%", mr)));
    }
    println!("{bot}");
}

// -- Scenario Runner ----------------------------------------------------------

async fn run_scenario(
    name: &str,
    config: &Config,
    client: &Client,
    order_fn: fn(&str, u64) -> Value,
) -> Result<ScenarioResult> {
    let is_verified = config.mode == Mode::Verified;

    println!("\n{}", "=".repeat(72));
    println!("  Scenario: {name}");
    println!(
        "  Mode:     {}",
        if is_verified {
            "verified (WebSocket trade counting)"
        } else {
            "sync (dispatch throughput)"
        }
    );
    println!("  Duration: {}s per level", config.duration_secs);
    println!("  Pairs:    {:?}", config.pairs);
    println!("{}", "=".repeat(72));

    if is_verified {
        println!(
            "{:>6}  {:>10}  {:>12}  {:>8}  {:>8}  {:>12}  {:>7}",
            "conc", "orders", "dispatch/s", "avg_ms", "errs", "v-trades/s", "match%"
        );
    } else {
        println!(
            "{:>6}  {:>10}  {:>12}  {:>8}  {:>8}  {:>8}",
            "conc", "orders", "dispatch/s", "avg_ms", "max_ms", "errs"
        );
    }
    println!("{}", "-".repeat(72));

    let mut scenario_result = ScenarioResult {
        label: config.label.clone(),
        scenario: name.to_string(),
        mode: if is_verified { "verified".to_string() } else { "sync".to_string() },
        levels: Vec::new(),
    };

    for &conc in &config.concurrency_levels {
        let stats = Stats::new();
        let barrier = Arc::new(Barrier::new(conc + 1));
        let deadline = Instant::now() + Duration::from_secs(config.duration_secs);

        // In verified mode: start WS trade counter, record trades before this level
        let ws_counter = if is_verified {
            let ws_url = config.ws_base_url();
            let pairs = config.pairs.clone();
            match start_ws_trade_counter(ws_url, pairs).await {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("[verified] WS connect failed: {e}  (falling back to sync)");
                    None
                }
            }
        } else {
            None
        };

        let trades_before = ws_counter
            .as_ref()
            .map(|(c, _)| c.load(Ordering::Relaxed))
            .unwrap_or(0);

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
                        Ok(()) => stats.record(start.elapsed()),
                        Err(e) => {
                            if stats.errors.load(std::sync::atomic::Ordering::Relaxed) < 5 {
                                eprintln!("[loadtest] error: {e:#}");
                            }
                            stats.record_error();
                        }
                    }
                    seq += 1;
                }
            }));
        }

        barrier.wait().await;
        let wall_start = Instant::now();

        for h in handles {
            let _ = h.await;
        }
        let wall_elapsed = wall_start.elapsed().as_secs_f64();

        // In verified mode: wait up to 5s for trades to drain through the pipeline
        let verified_trades = if let Some((ref trade_count, _)) = ws_counter {
            let drain_deadline = Instant::now() + Duration::from_secs(5);
            let mut last = trade_count.load(Ordering::Relaxed);
            while Instant::now() < drain_deadline {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let current = trade_count.load(Ordering::Relaxed);
                if current > last {
                    last = current;
                } else {
                    break; // no new trades for 200ms — pipeline drained
                }
            }
            Some(trade_count.load(Ordering::Relaxed) - trades_before)
        } else {
            None
        };

        // Cancel the WS counter for this level
        if let Some((_, cancel)) = ws_counter {
            let _ = cancel.send(());
        }

        let (orders, errors, lat_sum, lat_max) = stats.snapshot();
        let dispatch_per_sec = orders as f64 / wall_elapsed;
        let avg_latency_ms = if orders > 0 {
            (lat_sum as f64 / orders as f64) / 1000.0
        } else {
            0.0
        };
        let max_latency_ms = lat_max as f64 / 1000.0;

        let (verified_trades_per_sec, match_rate_pct) = if let Some(vt) = verified_trades {
            let tps = vt as f64 / wall_elapsed;
            // crossing scenario: each pair of orders should produce 1 trade
            // match rate = trades / (orders / 2)
            let rate = if orders > 0 {
                (vt as f64 / (orders as f64 / 2.0)) * 100.0
            } else {
                0.0
            };
            (Some(tps), Some(rate))
        } else {
            (None, None)
        };

        if is_verified {
            println!(
                "{:>6}  {:>10}  {:>12.1}  {:>8.2}  {:>8}  {:>12.1}  {:>7.1}",
                conc,
                orders,
                dispatch_per_sec,
                avg_latency_ms,
                errors,
                verified_trades_per_sec.unwrap_or(0.0),
                match_rate_pct.unwrap_or(0.0),
            );
        } else {
            println!(
                "{:>6}  {:>10}  {:>12.1}  {:>8.2}  {:>8.1}  {:>8}",
                conc,
                orders,
                dispatch_per_sec,
                avg_latency_ms,
                max_latency_ms,
                errors,
            );
        }

        scenario_result.levels.push(LevelResult {
            concurrency: conc,
            orders,
            errors,
            dispatch_per_sec,
            avg_latency_ms,
            max_latency_ms,
            verified_trades,
            verified_trades_per_sec,
            match_rate_pct,
        });
    }

    // Print summary for the highest concurrency level
    if let Some(last) = scenario_result.levels.last() {
        println!();
        print_summary(&config.label, name, last);
    }

    Ok(scenario_result)
}

// -- Main ---------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_args();

    println!("SME Load Test");
    println!("  URL: {}", config.base_url);
    println!(
        "  Mode: {}",
        match config.mode {
            Mode::Sync => "sync (dispatch throughput)",
            Mode::Verified => "verified (WebSocket trade counting)",
        }
    );
    println!("  Label: {}", config.label);
    println!("  Concurrency: {:?}", config.concurrency_levels);
    println!("  Duration: {}s per level", config.duration_secs);

    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(128)
        .build()?;

    if let Err(e) = warm_up(&client, &config.base_url).await {
        eprintln!("[warn] warm_up failed: {e}");
    }

    let mut all_results: Vec<ScenarioResult> = Vec::new();

    for scenario in &config.scenarios {
        let result = match scenario.as_str() {
            "resting" => {
                run_scenario(
                    "Resting Orders (IOC, no matching)",
                    &config,
                    &client,
                    resting_order,
                )
                .await?
            }
            "crossing" => {
                run_scenario(
                    "Crossing Orders (GTC, every 2nd matches)",
                    &config,
                    &client,
                    crossing_order,
                )
                .await?
            }
            "mixed" => {
                run_scenario(
                    "Mixed (50% resting, 50% crossing)",
                    &config,
                    &client,
                    mixed_order,
                )
                .await?
            }
            "multi-pair" => {
                if config.pairs.len() < 2 {
                    println!("\n  [skip multi-pair: need --pairs BTC-USDT,ETH-USDT,...]");
                    continue;
                }
                run_scenario(
                    "Multi-Pair Crossing",
                    &config,
                    &client,
                    crossing_order,
                )
                .await?
            }
            _ => {
                println!("Unknown scenario: {scenario}");
                continue;
            }
        };
        all_results.push(result);
    }

    // Optionally write JSON output
    if let Some(ref path) = config.output {
        let json = serde_json::to_string_pretty(&all_results)?;
        std::fs::write(path, json)?;
        println!("\nResults written to: {path}");
    }

    println!("\nLoad test complete.");
    Ok(())
}
