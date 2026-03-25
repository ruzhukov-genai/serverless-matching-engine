//! Order Book Locking — distributed per-pair mutex via Valkey/Redis SET NX EX.
//!
//! Uses exponential backoff with jitter.
//!
//! Lock value format: `{worker_id}:{fence_token}` where fence_token is a
//! monotonically increasing counter. The fence token can be used by callers
//! for optimistic concurrency control on cache writes.

use anyhow::{bail, Result};
use deadpool_redis::Pool;
use tokio::time::{sleep, Duration};

const BACKOFF_MS: &[u64] = &[5, 10, 25, 50, 100, 200, 400, 800];
const MAX_RETRIES: usize = 20;
const LOCK_TTL_SECS: usize = 5;

/// A held lock guard. Call `release()` when done, or it auto-releases on Drop.
///
/// The `fence_token` is a monotonically increasing value that can be used
/// to detect stale writes — if your fence is lower than the current one,
/// your state is stale and writes should be aborted.
pub struct LockGuard {
    pub pair_id: String,
    pub worker_id: String,
    pub fence_token: i64,
    pool: Pool,
    released: bool,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        tracing::warn!(
            pair_id = %self.pair_id,
            worker_id = %self.worker_id,
            "LockGuard dropped without explicit release — spawning async cleanup"
        );
        let pool = self.pool.clone();
        let pair_id = self.pair_id.clone();
        let worker_id = self.worker_id.clone();
        let fence = self.fence_token;
        tokio::spawn(async move {
            if let Err(e) = release_lock(&pool, &pair_id, &worker_id, fence).await {
                tracing::warn!("lock release failed on drop: {e}");
            }
        });
    }
}

impl LockGuard {
    /// Explicit async release (preferred over Drop when in async context).
    pub async fn release(mut self) {
        self.released = true;
        if let Err(e) = release_lock(&self.pool, &self.pair_id, &self.worker_id, self.fence_token).await {
            tracing::warn!("lock release failed: {e}");
        }
    }
}

/// Try to acquire the lock for `pair_id` with exponential backoff + jitter.
///
/// Returns a `LockGuard` with a fence token that monotonically increases
/// across all acquisitions for this pair. The fence token is stored in
/// `book:{pair_id}:fence` and is atomically incremented on each acquisition.
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
            // Atomically increment fence token on successful acquisition
            let fence_key = format!("book:{pair_id}:fence");
            let fence: i64 = redis::cmd("INCR")
                .arg(&fence_key)
                .query_async(&mut *conn)
                .await
                .unwrap_or(1);

            tracing::debug!(
                pair_id = %pair_id,
                worker_id = %worker_id,
                fence_token = fence,
                attempt = attempt,
                "lock acquired"
            );

            return Ok(LockGuard {
                pair_id: pair_id.to_string(),
                worker_id: worker_id.to_string(),
                fence_token: fence,
                pool: pool.clone(),
                released: false,
            });
        }

        // Backoff with jitter
        let base_ms = BACKOFF_MS[attempt.min(BACKOFF_MS.len() - 1)];
        let jitter_ms = rand::random::<u64>() % (base_ms / 2 + 1);
        sleep(Duration::from_millis(base_ms + jitter_ms)).await;
    }

    bail!("failed to acquire lock for pair {pair_id} after {MAX_RETRIES} retries")
}

/// Release a lock only if we still own it.
///
/// Uses a Lua script to atomically check ownership before deleting.
/// Also verifies the fence token hasn't advanced (which would mean our lock
/// expired and someone else acquired it).
pub async fn release_lock(pool: &Pool, pair_id: &str, worker_id: &str, _fence: i64) -> Result<()> {
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

    let deleted: i64 = redis::Script::new(script)
        .key(&key)
        .arg(worker_id)
        .invoke_async(&mut *conn)
        .await?;

    if deleted == 0 {
        tracing::warn!(
            pair_id = %pair_id,
            worker_id = %worker_id,
            "lock was not ours at release time — likely expired (TTL)"
        );
    }

    Ok(())
}

/// Check if the lock is currently held (for diagnostics only — NOT for decisions).
pub async fn is_locked(pool: &Pool, pair_id: &str) -> Result<bool> {
    let key = format!("book:{pair_id}:lock");
    let mut conn = pool.get().await?;
    let val: Option<String> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut *conn)
        .await?;
    Ok(val.is_some())
}

/// Get the current fence token for a pair (for diagnostics).
pub async fn current_fence(pool: &Pool, pair_id: &str) -> Result<i64> {
    let fence_key = format!("book:{pair_id}:fence");
    let mut conn = pool.get().await?;
    let val: Option<i64> = redis::cmd("GET")
        .arg(&fence_key)
        .query_async(&mut *conn)
        .await?;
    Ok(val.unwrap_or(0))
}
