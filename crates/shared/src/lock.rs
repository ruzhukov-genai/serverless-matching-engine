//! Order Book Locking — distributed per-pair mutex via Dragonfly/Redis SET NX EX.
//!
//! Uses exponential backoff with jitter, max 10 retries.

use anyhow::{bail, Result};
use deadpool_redis::Pool;
use rand::Rng;
use tokio::time::{sleep, Duration};

const BACKOFF_MS: &[u64] = &[10, 20, 50, 100, 200, 500];
const MAX_RETRIES: usize = 10;
const LOCK_TTL_SECS: usize = 1;

/// A held lock guard. Call `release()` when done, or it auto-releases on Drop.
pub struct LockGuard {
    pub pair_id: String,
    pub worker_id: String,
    pool: Pool,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let pool = self.pool.clone();
        let pair_id = self.pair_id.clone();
        let worker_id = self.worker_id.clone();
        tokio::spawn(async move {
            if let Err(e) = release_lock(&pool, &pair_id, &worker_id).await {
                tracing::warn!("lock release failed on drop: {e}");
            }
        });
    }
}

impl LockGuard {
    /// Explicit async release (preferred over Drop when in async context).
    pub async fn release(self) {
        let pool = self.pool.clone();
        let pair_id = self.pair_id.clone();
        let worker_id = self.worker_id.clone();
        std::mem::forget(self); // prevent double-release from Drop
        if let Err(e) = release_lock(&pool, &pair_id, &worker_id).await {
            tracing::warn!("lock release failed: {e}");
        }
    }
}

/// Try to acquire the lock for `pair_id` with exponential backoff + jitter.
pub async fn acquire_lock(pool: &Pool, pair_id: &str, worker_id: &str) -> Result<LockGuard> {
    let key = format!("book:{pair_id}:lock");
    for attempt in 0..MAX_RETRIES {
        let mut conn = pool.get().await?;
        let result: Option<String> = redis::cmd("SET")
            .arg(&key)
            .arg(worker_id)
            .arg("NX")
            .arg("EX")
            .arg(LOCK_TTL_SECS)
            .query_async(&mut *conn)
            .await?;

        if result.is_some() {
            return Ok(LockGuard {
                pair_id: pair_id.to_string(),
                worker_id: worker_id.to_string(),
                pool: pool.clone(),
            });
        }

        // backoff with jitter
        let base_ms = BACKOFF_MS[attempt.min(BACKOFF_MS.len() - 1)];
        let jitter_ms = rand::random::<u64>() % (base_ms / 2 + 1);
        sleep(Duration::from_millis(base_ms + jitter_ms)).await;
    }

    bail!("failed to acquire lock for pair {pair_id} after {MAX_RETRIES} retries")
}

/// Release a lock only if we still own it (GET + compare + DEL via Lua).
pub async fn release_lock(pool: &Pool, pair_id: &str, worker_id: &str) -> Result<()> {
    let key = format!("book:{pair_id}:lock");
    let mut conn = pool.get().await?;

    // Lua script: only DEL if the value matches our worker_id
    let script = r#"
        local val = redis.call('GET', KEYS[1])
        if val == ARGV[1] then
            return redis.call('DEL', KEYS[1])
        end
        return 0
    "#;

    let _: redis::Value = redis::Script::new(script)
        .key(&key)
        .arg(worker_id)
        .invoke_async(&mut *conn)
        .await?;

    Ok(())
}
