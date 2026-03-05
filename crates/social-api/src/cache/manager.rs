use bb8::Pool;
use bb8_redis::RedisConnectionManager;
use redis::AsyncCommands;
use std::time::Duration;

use crate::config::Config;

/// Redis cache manager with graceful degradation.
///
/// All operations catch Redis errors and return None/Ok instead of propagating.
/// The service continues operating with degraded performance when Redis is down.
pub type RedisPool = Pool<RedisConnectionManager>;

/// Create a bb8 Redis connection pool from config.
pub async fn create_pool(config: &Config) -> Result<RedisPool, Box<dyn std::error::Error>> {
    let manager = RedisConnectionManager::new(config.redis_url.as_str())?;

    let pool = Pool::builder()
        .max_size(config.redis_pool_size)
        .min_idle(Some(1))
        .connection_timeout(Duration::from_secs(3))
        .build(manager)
        .await?;

    tracing::info!(pool_size = config.redis_pool_size, "Redis pool initialized");
    Ok(pool)
}

/// Cache manager wrapping Redis pool with graceful fallback.
///
/// Every public method returns Option/Result that never propagates Redis errors
/// to the caller. Errors are logged and treated as cache misses.
#[derive(Clone)]
pub struct CacheManager {
    pool: RedisPool,
}

impl CacheManager {
    pub fn new(pool: RedisPool) -> Self {
        Self { pool }
    }

    /// Get a string value by key. Returns None on miss or Redis error.
    pub async fn get(&self, key: &str) -> Option<String> {
        let mut conn = self.pool.get().await.ok()?;
        match conn.get::<_, Option<String>>(key).await {
            Ok(val) => val,
            Err(e) => {
                tracing::warn!(key = key, error = %e, "Redis GET failed");
                None
            }
        }
    }

    /// Set a string value with TTL. Silently fails on Redis error.
    pub async fn set(&self, key: &str, value: &str, ttl_secs: u64) {
        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(key = key, error = %e, "Redis pool GET failed");
                return;
            }
        };
        if let Err(e) = conn.set_ex::<_, _, ()>(key, value, ttl_secs).await {
            tracing::warn!(key = key, error = %e, "Redis SET failed");
        }
    }

    /// Delete a key. Silently fails on Redis error.
    pub async fn del(&self, key: &str) {
        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(key = key, error = %e, "Redis pool GET failed");
                return;
            }
        };
        if let Err(e) = conn.del::<_, ()>(key).await {
            tracing::warn!(key = key, error = %e, "Redis DEL failed");
        }
    }

    /// Increment a key atomically. Returns new value, or None on error.
    pub async fn incr(&self, key: &str) -> Option<i64> {
        let mut conn = self.pool.get().await.ok()?;
        match conn.incr::<_, _, i64>(key, 1).await {
            Ok(val) => Some(val),
            Err(e) => {
                tracing::warn!(key = key, error = %e, "Redis INCR failed");
                None
            }
        }
    }

    /// Decrement a key atomically. Returns new value, or None on error.
    pub async fn decr(&self, key: &str) -> Option<i64> {
        let mut conn = self.pool.get().await.ok()?;
        match conn.decr::<_, _, i64>(key, 1).await {
            Ok(val) => Some(val),
            Err(e) => {
                tracing::warn!(key = key, error = %e, "Redis DECR failed");
                None
            }
        }
    }

    /// Set a key with TTL only if it doesn't exist (for stampede lock).
    /// Returns true if the lock was acquired.
    pub async fn set_nx(&self, key: &str, value: &str, ttl_secs: u64) -> bool {
        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(_) => return false,
        };

        // SET key value NX EX ttl
        let result: Result<bool, _> = redis::cmd("SET")
            .arg(key)
            .arg(value)
            .arg("NX")
            .arg("EX")
            .arg(ttl_secs)
            .query_async(&mut *conn)
            .await;

        match result {
            Ok(acquired) => acquired,
            Err(e) => {
                tracing::warn!(key = key, error = %e, "Redis SET NX failed");
                false
            }
        }
    }

    /// Get multiple keys at once (MGET). Returns Vec with None for misses.
    pub async fn mget(&self, keys: &[String]) -> Vec<Option<String>> {
        if keys.is_empty() {
            return vec![];
        }

        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Redis pool GET failed for MGET");
                return vec![None; keys.len()];
            }
        };

        match conn.get::<_, Vec<Option<String>>>(keys).await {
            Ok(vals) => vals,
            Err(e) => {
                tracing::warn!(error = %e, "Redis MGET failed");
                vec![None; keys.len()]
            }
        }
    }

    /// Execute a Lua script. Returns the raw redis Value, or None on error.
    pub async fn eval_script(
        &self,
        script: &str,
        keys: &[&str],
        args: &[&str],
    ) -> Option<redis::Value> {
        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Redis pool GET failed for script");
                return None;
            }
        };

        let mut cmd = redis::cmd("EVAL");
        cmd.arg(script).arg(keys.len());
        for k in keys {
            cmd.arg(*k);
        }
        for a in args {
            cmd.arg(*a);
        }

        match cmd.query_async::<redis::Value>(&mut *conn).await {
            Ok(val) => Some(val),
            Err(e) => {
                tracing::warn!(error = %e, "Redis EVAL failed");
                None
            }
        }
    }

    /// Publish a message to a Redis channel.
    pub async fn publish(&self, channel: &str, message: &str) {
        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(channel = channel, error = %e, "Redis pool GET failed for PUBLISH");
                return;
            }
        };
        if let Err(e) = conn.publish::<_, _, ()>(channel, message).await {
            tracing::warn!(channel = channel, error = %e, "Redis PUBLISH failed");
        }
    }

    /// Check if Redis is reachable (for health checks).
    pub async fn is_healthy(&self) -> bool {
        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(_) => return false,
        };
        redis::cmd("PING")
            .query_async::<String>(&mut *conn)
            .await
            .is_ok()
    }

    /// Get the underlying pool for Pub/Sub subscriber connections.
    pub fn pool(&self) -> &RedisPool {
        &self.pool
    }
}
