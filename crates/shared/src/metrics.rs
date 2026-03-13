//! Dragonfly-backed metrics helpers for the matching engine.
//!
//! Keys:
//!   metrics:{pair_id}:latency   → list of match latency samples (ms, newest first)
//!   metrics:{pair_id}:lock_wait → list of lock-wait latency samples (ms, newest first)
//!   metrics:{pair_id}:orders    → INCR counter for orders processed
//!   metrics:{pair_id}:trades    → INCRBY counter for trades executed

use anyhow::{Context, Result};
use deadpool_redis::Pool;

const SAMPLE_CAP: isize = 1000;

// ── Write helpers ─────────────────────────────────────────────────────────────

/// Push a match latency sample (ms) to the list; trim to last 1000.
pub async fn record_match_latency(pool: &Pool, pair_id: &str, ms: u64) -> Result<()> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = format!("metrics:{pair_id}:latency");
    let () = redis::pipe()
        .cmd("LPUSH")
        .arg(&key)
        .arg(ms.to_string())
        .cmd("LTRIM")
        .arg(&key)
        .arg(0)
        .arg(SAMPLE_CAP - 1)
        .query_async(&mut *conn)
        .await
        .context("record_match_latency")?;
    Ok(())
}

/// Push a lock-wait latency sample (ms) to the list; trim to last 1000.
pub async fn record_lock_wait(pool: &Pool, pair_id: &str, ms: u64) -> Result<()> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = format!("metrics:{pair_id}:lock_wait");
    let () = redis::pipe()
        .cmd("LPUSH")
        .arg(&key)
        .arg(ms.to_string())
        .cmd("LTRIM")
        .arg(&key)
        .arg(0)
        .arg(SAMPLE_CAP - 1)
        .query_async(&mut *conn)
        .await
        .context("record_lock_wait")?;
    Ok(())
}

/// Increment per-pair order counter.
pub async fn increment_order_count(pool: &Pool, pair_id: &str) -> Result<i64> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = format!("metrics:{pair_id}:orders");
    let v: i64 = redis::cmd("INCR")
        .arg(&key)
        .query_async(&mut *conn)
        .await
        .context("increment_order_count")?;
    Ok(v)
}

/// Add `count` to the per-pair trade counter.
pub async fn increment_trade_count(pool: &Pool, pair_id: &str, count: u64) -> Result<i64> {
    if count == 0 {
        return Ok(0);
    }
    let mut conn = pool.get().await.context("pool.get")?;
    let key = format!("metrics:{pair_id}:trades");
    let v: i64 = redis::cmd("INCRBY")
        .arg(&key)
        .arg(count)
        .query_async(&mut *conn)
        .await
        .context("increment_trade_count")?;
    Ok(v)
}

// ── Read helpers ──────────────────────────────────────────────────────────────

/// Get total order count for a pair.
pub async fn get_order_count(pool: &Pool, pair_id: &str) -> Result<i64> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = format!("metrics:{pair_id}:orders");
    let v: Option<i64> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut *conn)
        .await
        .context("get_order_count")?;
    Ok(v.unwrap_or(0))
}

/// Get total trade count for a pair.
pub async fn get_trade_count(pool: &Pool, pair_id: &str) -> Result<i64> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = format!("metrics:{pair_id}:trades");
    let v: Option<i64> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut *conn)
        .await
        .context("get_trade_count")?;
    Ok(v.unwrap_or(0))
}

/// Get the last `count` latency samples (ms) for a pair.
pub async fn get_latency_samples(pool: &Pool, pair_id: &str, count: isize) -> Result<Vec<u64>> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = format!("metrics:{pair_id}:latency");
    let raw: Vec<String> = redis::cmd("LRANGE")
        .arg(&key)
        .arg(0)
        .arg(count - 1)
        .query_async(&mut *conn)
        .await
        .context("get_latency_samples")?;
    Ok(raw.into_iter().filter_map(|s| s.parse().ok()).collect())
}

/// Get the last `count` lock-wait samples (ms) for a pair.
pub async fn get_lock_wait_samples(pool: &Pool, pair_id: &str, count: isize) -> Result<Vec<u64>> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = format!("metrics:{pair_id}:lock_wait");
    let raw: Vec<String> = redis::cmd("LRANGE")
        .arg(&key)
        .arg(0)
        .arg(count - 1)
        .query_async(&mut *conn)
        .await
        .context("get_lock_wait_samples")?;
    Ok(raw.into_iter().filter_map(|s| s.parse().ok()).collect())
}

// ── Percentile computation ────────────────────────────────────────────────────

/// Compute P50, P95, P99 from a sorted list of samples.
/// Returns (p50, p95, p99) — all as f64 ms.
pub fn compute_percentiles(mut samples: Vec<u64>) -> (f64, f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    samples.sort_unstable();
    let n = samples.len();
    let percentile = |pct: f64| -> f64 {
        let idx = ((pct / 100.0) * (n as f64 - 1.0)).round() as usize;
        samples[idx.min(n - 1)] as f64
    };
    (percentile(50.0), percentile(95.0), percentile(99.0))
}
