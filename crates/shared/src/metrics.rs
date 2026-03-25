//! Valkey-backed metrics helpers for the matching engine.
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

/// Push a sample to a metrics list; trim to last 1000.
async fn push_sample(pool: &Pool, pair_id: &str, metric_type: &str, ms: u64) -> Result<()> {
    let mut conn = pool.get().await.context("pool.get")?;
    let key = format!("metrics:{pair_id}:{metric_type}");
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
        .with_context(|| format!("push_sample {metric_type}"))?;
    Ok(())
}

/// Push a match latency sample (ms) to the list; trim to last 1000.
pub async fn record_match_latency(pool: &Pool, pair_id: &str, ms: u64) -> Result<()> {
    push_sample(pool, pair_id, "latency", ms).await
}

/// Push a lock-wait latency sample (ms) to the list; trim to last 1000.
pub async fn record_lock_wait(pool: &Pool, pair_id: &str, ms: u64) -> Result<()> {
    push_sample(pool, pair_id, "lock_wait", ms).await
}

/// Increment per-pair order counter.
pub async fn increment_order_count(pool: &Pool, pair_id: &str) -> Result<i64> {
    increment_order_count_by(pool, pair_id, 1).await
}

/// Add `count` to the per-pair order counter (batched variant).
pub async fn increment_order_count_by(pool: &Pool, pair_id: &str, count: u64) -> Result<i64> {
    if count == 0 {
        return Ok(0);
    }
    let mut conn = pool.get().await.context("pool.get")?;
    let key = format!("metrics:{pair_id}:orders");
    let v: i64 = redis::cmd("INCRBY")
        .arg(&key)
        .arg(count)
        .query_async(&mut *conn)
        .await
        .context("increment_order_count_by")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_empty() {
        let (p50, p95, p99) = compute_percentiles(vec![]);
        assert_eq!(p50, 0.0);
        assert_eq!(p95, 0.0);
        assert_eq!(p99, 0.0);
    }

    #[test]
    fn percentiles_single() {
        let (p50, p95, p99) = compute_percentiles(vec![42]);
        assert_eq!(p50, 42.0);
        assert_eq!(p95, 42.0);
        assert_eq!(p99, 42.0);
    }

    #[test]
    fn percentiles_two_values() {
        let (p50, p95, p99) = compute_percentiles(vec![10, 20]);
        assert_eq!(p50, 20.0); // idx = round(0.5 * 1) = 1
        assert_eq!(p95, 20.0);
        assert_eq!(p99, 20.0);
    }

    #[test]
    fn percentiles_100_values() {
        // 1..=100 → idx = round(pct/100 * 99)
        let samples: Vec<u64> = (1..=100).collect();
        let (p50, p95, p99) = compute_percentiles(samples);
        // p50: round(0.5 * 99) = 50 → samples[49]=50 (0-indexed) or samples[50]=51
        // Actual: round(49.5) = 50 → samples[50] = 51
        assert_eq!(p50, 51.0);
        assert_eq!(p95, 95.0);  // round(94.05) = 94 → samples[94] = 95
        assert_eq!(p99, 99.0);  // round(98.01) = 98 → samples[98] = 99
    }

    #[test]
    fn percentiles_unsorted_input() {
        // Should sort internally
        let samples = vec![100, 1, 50, 75, 25, 90, 10, 5, 95, 99];
        let (p50, p95, p99) = compute_percentiles(samples);
        assert!(p50 > 0.0);
        assert!(p95 >= p50);
        assert!(p99 >= p95);
    }

    #[test]
    fn percentiles_all_same() {
        let samples = vec![7, 7, 7, 7, 7];
        let (p50, p95, p99) = compute_percentiles(samples);
        assert_eq!(p50, 7.0);
        assert_eq!(p95, 7.0);
        assert_eq!(p99, 7.0);
    }

    #[test]
    fn percentiles_large_outlier() {
        // 99 values of 1, one value of 1000 (at index 99 after sort)
        let mut samples: Vec<u64> = vec![1; 99];
        samples.push(1000);
        let (p50, p95, p99) = compute_percentiles(samples);
        assert_eq!(p50, 1.0);
        // p95: round(0.95 * 99) = round(94.05) = 94 → samples[94] = 1
        assert_eq!(p95, 1.0);
        // p99: round(0.99 * 99) = round(98.01) = 98 → samples[98] = 1 (outlier is at 99)
        assert_eq!(p99, 1.0);
        // The outlier at index 99 is only hit at P100
    }
}
