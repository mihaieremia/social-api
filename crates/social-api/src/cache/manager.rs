use bb8::Pool;
use bb8_redis::RedisConnectionManager;
use metrics::counter;
use redis::AsyncCommands;
use std::sync::LazyLock;
use std::time::Duration;

use crate::config::Config;

/// Redis cache manager with graceful degradation.
///
/// All operations catch Redis errors and return None/Ok instead of propagating.
/// The service continues operating with degraded performance when Redis is down.
pub type RedisPool = Pool<RedisConnectionManager>;

/// Lua script: atomically INCR an existing key or SET from DB count if missing.
/// Prevents ghost counts when INCR is called on an expired/non-existent key.
static CONDITIONAL_INCR_SCRIPT: LazyLock<redis::Script> = LazyLock::new(|| {
    redis::Script::new(
        r#"
local key = KEYS[1]
local ttl = tonumber(ARGV[1])
local db_count = tonumber(ARGV[2])

if redis.call('EXISTS', key) == 1 then
  local val = redis.call('INCRBY', key, 1)
  redis.call('EXPIRE', key, ttl)
  return val
else
  redis.call('SET', key, db_count, 'EX', ttl)
  return db_count
end
"#,
    )
});

/// Lua script: atomically DECR an existing key (floor at 0) or SET from DB count.
static CONDITIONAL_DECR_SCRIPT: LazyLock<redis::Script> = LazyLock::new(|| {
    redis::Script::new(
        r#"
local key = KEYS[1]
local ttl = tonumber(ARGV[1])
local db_count = tonumber(ARGV[2])

if redis.call('EXISTS', key) == 1 then
  local val = redis.call('DECRBY', key, 1)
  if tonumber(val) < 0 then
    redis.call('SET', key, 0, 'EX', ttl)
    return 0
  end
  redis.call('EXPIRE', key, ttl)
  return val
else
  redis.call('SET', key, db_count, 'EX', ttl)
  return db_count
end
"#,
    )
});

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
            Ok(Some(val)) => {
                counter!("social_api_cache_operations_total", "operation" => "get", "result" => "hit").increment(1);
                Some(val)
            }
            Ok(None) => {
                counter!("social_api_cache_operations_total", "operation" => "get", "result" => "miss").increment(1);
                None
            }
            Err(e) => {
                counter!("social_api_cache_operations_total", "operation" => "get", "result" => "error").increment(1);
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
                counter!("social_api_cache_operations_total", "operation" => "set", "result" => "error").increment(1);
                tracing::warn!(key = key, error = %e, "Redis pool GET failed");
                return;
            }
        };
        match conn.set_ex::<_, _, ()>(key, value, ttl_secs).await {
            Ok(()) => {
                counter!("social_api_cache_operations_total", "operation" => "set", "result" => "ok").increment(1);
            }
            Err(e) => {
                counter!("social_api_cache_operations_total", "operation" => "set", "result" => "error").increment(1);
                tracing::warn!(key = key, error = %e, "Redis SET failed");
            }
        }
    }

    /// Conditionally INCR: if key exists, INCR + refresh TTL. If missing, SET to db_count.
    /// Prevents ghost counts when INCR hits an expired key.
    pub async fn conditional_incr(&self, key: &str, ttl_secs: u64, db_count: i64) -> Option<i64> {
        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(key = key, error = %e, "Redis pool GET failed for conditional_incr");
                return None;
            }
        };

        let ttl_str = ttl_secs.to_string();
        let db_count_str = db_count.to_string();

        match CONDITIONAL_INCR_SCRIPT
            .key(key)
            .arg(ttl_str)
            .arg(db_count_str)
            .invoke_async::<i64>(&mut *conn)
            .await
        {
            Ok(val) => {
                counter!("social_api_cache_operations_total", "operation" => "conditional_incr", "result" => "ok").increment(1);
                Some(val)
            }
            Err(e) => {
                counter!("social_api_cache_operations_total", "operation" => "conditional_incr", "result" => "error").increment(1);
                tracing::warn!(key = key, error = %e, "Redis conditional INCR script failed");
                None
            }
        }
    }

    /// Conditionally DECR: if key exists, DECR (floor 0) + refresh TTL. If missing, SET to db_count.
    pub async fn conditional_decr(&self, key: &str, ttl_secs: u64, db_count: i64) -> Option<i64> {
        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(key = key, error = %e, "Redis pool GET failed for conditional_decr");
                return None;
            }
        };

        let ttl_str = ttl_secs.to_string();
        let db_count_str = db_count.to_string();

        match CONDITIONAL_DECR_SCRIPT
            .key(key)
            .arg(ttl_str)
            .arg(db_count_str)
            .invoke_async::<i64>(&mut *conn)
            .await
        {
            Ok(val) => {
                counter!("social_api_cache_operations_total", "operation" => "conditional_decr", "result" => "ok").increment(1);
                Some(val)
            }
            Err(e) => {
                counter!("social_api_cache_operations_total", "operation" => "conditional_decr", "result" => "error").increment(1);
                tracing::warn!(key = key, error = %e, "Redis conditional DECR script failed");
                None
            }
        }
    }

    /// Invoke a `redis::Script` (auto EVALSHA). Returns raw redis Value, or None on error.
    pub async fn invoke_script(
        &self,
        script: &redis::Script,
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

        let mut invocation = script.prepare_invoke();
        for k in keys {
            invocation.key(*k);
        }
        for a in args {
            invocation.arg(*a);
        }

        match invocation.invoke_async::<redis::Value>(&mut *conn).await {
            Ok(val) => Some(val),
            Err(e) => {
                tracing::warn!(error = %e, "Redis script EVALSHA/EVAL failed");
                None
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
                counter!("social_api_cache_operations_total", "operation" => "mget", "result" => "error").increment(1);
                tracing::warn!(error = %e, "Redis pool GET failed for MGET");
                return vec![None; keys.len()];
            }
        };

        match conn.get::<_, Vec<Option<String>>>(keys).await {
            Ok(vals) => {
                let hits = vals.iter().filter(|v| v.is_some()).count();
                let misses = vals.len() - hits;
                if hits > 0 {
                    counter!("social_api_cache_operations_total", "operation" => "mget", "result" => "hit").increment(hits as u64);
                }
                if misses > 0 {
                    counter!("social_api_cache_operations_total", "operation" => "mget", "result" => "miss").increment(misses as u64);
                }
                vals
            }
            Err(e) => {
                counter!("social_api_cache_operations_total", "operation" => "mget", "result" => "error").increment(1);
                tracing::warn!(error = %e, "Redis MGET failed");
                vec![None; keys.len()]
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

    /// Replace a sorted set atomically: DEL the old key then ZADD all members.
    /// Used by the leaderboard refresh task.
    pub async fn replace_sorted_set(&self, key: &str, members: &[(String, f64)]) {
        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(key = key, error = %e, "Redis pool GET failed for ZADD");
                return;
            }
        };

        // Pipeline: DEL then ZADD
        let mut pipe = redis::pipe();
        pipe.atomic();
        pipe.del(key).ignore();

        if !members.is_empty() {
            let mut zadd = redis::cmd("ZADD");
            zadd.arg(key);
            for (member, score) in members {
                zadd.arg(*score).arg(member.as_str());
            }
            pipe.add_command(zadd).ignore();
        }

        if let Err(e) = pipe.query_async::<()>(&mut *conn).await {
            tracing::warn!(key = key, error = %e, "Redis ZADD pipeline failed");
        }
    }

    /// Read the top entries from a sorted set in descending score order.
    /// Returns `(member, score)` pairs, or an empty vec on miss/error.
    pub async fn zrevrange_with_scores(
        &self,
        key: &str,
        start: isize,
        stop: isize,
    ) -> Vec<(String, f64)> {
        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(key = key, error = %e, "Redis pool GET failed for ZREVRANGE");
                return vec![];
            }
        };

        match conn
            .zrevrange_withscores::<_, Vec<(String, f64)>>(key, start, stop)
            .await
        {
            Ok(pairs) => pairs,
            Err(e) => {
                tracing::warn!(key = key, error = %e, "Redis ZREVRANGE failed");
                vec![]
            }
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

}
