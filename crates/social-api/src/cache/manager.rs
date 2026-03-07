use bb8::Pool;
use bb8_redis::RedisConnectionManager;
use metrics::counter;
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
        .idle_timeout(Some(Duration::from_secs(300)))
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

    /// Set multiple string values with TTL in a single Redis pipeline (SETEX per key).
    /// Silently fails on Redis error. Used by batch_counts to cache misses in one round-trip.
    pub async fn set_many(&self, entries: &[(&str, &str, u64)]) {
        if entries.is_empty() {
            return;
        }
        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                counter!("social_api_cache_operations_total", "operation" => "set_many", "result" => "error").increment(1);
                tracing::warn!(error = %e, "Redis pool GET failed for set_many");
                return;
            }
        };
        let mut pipe = redis::pipe();
        for (key, value, ttl) in entries {
            pipe.cmd("SETEX").arg(*key).arg(*ttl).arg(*value).ignore();
        }
        match pipe.query_async::<()>(&mut *conn).await {
            Ok(()) => {
                counter!("social_api_cache_operations_total", "operation" => "set_many", "result" => "ok").increment(entries.len() as u64);
            }
            Err(e) => {
                counter!("social_api_cache_operations_total", "operation" => "set_many", "result" => "error").increment(1);
                tracing::warn!(error = %e, "Redis set_many pipeline failed");
            }
        }
    }

    /// Delete a key. Silently fails on Redis error.
    /// Used only by integration tests (cache flush for cold-cache benchmarks).
    #[allow(dead_code)]
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

        match redis::cmd("MGET")
            .arg(keys)
            .query_async::<Vec<Option<String>>>(&mut *conn)
            .await
        {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Return a CacheManager connected to the shared Redis container.
    async fn make_cache() -> CacheManager {
        let redis = crate::test_containers::shared_redis().await;
        let mut config = crate::config::Config::new_for_test();
        config.redis_url = redis.url.clone();
        let pool = create_pool(&config).await.unwrap();
        CacheManager::new(pool)
    }

    // -------------------------------------------------------------------------
    // 1. get / set round-trip
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_set() {
        let cache = make_cache().await;
        let key = "test:get_set";

        // Nothing there yet
        assert_eq!(cache.get(key).await, None);

        // Set, then retrieve
        cache.set(key, "hello", 60).await;
        assert_eq!(cache.get(key).await, Some("hello".to_string()));
    }

    // -------------------------------------------------------------------------
    // 2. get missing key returns None
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_missing_key_returns_none() {
        let cache = make_cache().await;
        assert_eq!(cache.get("test:definitely_absent_key").await, None);
    }

    // -------------------------------------------------------------------------
    // 3. mget returns hits, misses, and empty input
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_mget() {
        let cache = make_cache().await;

        cache.set("test:mget_k1", "100", 60).await;
        cache.set("test:mget_k2", "200", 60).await;

        let keys: Vec<String> = vec![
            "test:mget_k1".to_string(),
            "test:mget_k2".to_string(),
            "test:mget_absent".to_string(),
        ];
        let results = cache.mget(&keys).await;

        assert_eq!(results.len(), 3);
        assert_eq!(results[0], Some("100".to_string()));
        assert_eq!(results[1], Some("200".to_string()));
        assert_eq!(results[2], None);
    }

    #[tokio::test]
    async fn test_mget_empty_input_returns_empty() {
        let cache = make_cache().await;
        let results = cache.mget(&[]).await;
        assert!(results.is_empty());
    }

    // -------------------------------------------------------------------------
    // 4. replace_sorted_set then zrevrange_with_scores returns descending order
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_zrevrange_with_scores() {
        let cache = make_cache().await;
        let key = "test:zrev_scores";

        let members = vec![
            ("post:aaa".to_string(), 10.0_f64),
            ("post:bbb".to_string(), 50.0_f64),
            ("post:ccc".to_string(), 30.0_f64),
        ];
        cache.replace_sorted_set(key, &members).await;

        let results = cache.zrevrange_with_scores(key, 0, -1).await;

        // Should be in descending score order: bbb(50), ccc(30), aaa(10)
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, "post:bbb");
        assert!((results[0].1 - 50.0).abs() < f64::EPSILON);
        assert_eq!(results[1].0, "post:ccc");
        assert!((results[1].1 - 30.0).abs() < f64::EPSILON);
        assert_eq!(results[2].0, "post:aaa");
        assert!((results[2].1 - 10.0).abs() < f64::EPSILON);
    }

    // -------------------------------------------------------------------------
    // 5. replace_sorted_set overwrites previous entries
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_replace_sorted_set_overwrites() {
        let cache = make_cache().await;
        let key = "test:zrev_overwrite";

        // First population
        let first = vec![
            ("old:entry1".to_string(), 100.0_f64),
            ("old:entry2".to_string(), 200.0_f64),
        ];
        cache.replace_sorted_set(key, &first).await;

        // Second population should replace, not merge
        let second = vec![("new:entry".to_string(), 999.0_f64)];
        cache.replace_sorted_set(key, &second).await;

        let results = cache.zrevrange_with_scores(key, 0, -1).await;
        assert_eq!(results.len(), 1, "old entries should be gone after replace");
        assert_eq!(results[0].0, "new:entry");
    }

    // -------------------------------------------------------------------------
    // 6. is_healthy returns true when Redis is reachable
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_is_healthy_returns_true() {
        let cache = make_cache().await;
        assert!(cache.is_healthy().await);
    }

    // -------------------------------------------------------------------------
    // 7. Redis-unavailable degradation: all methods return safe defaults
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_redis_unavailable_returns_safe_defaults() {
        // Point at a port that has no Redis; use a very short connection timeout
        // so the test completes quickly.
        let manager = bb8_redis::RedisConnectionManager::new("redis://127.0.0.1:19998").unwrap();
        let pool = bb8::Pool::builder()
            .max_size(2)
            .connection_timeout(std::time::Duration::from_millis(50))
            .build(manager)
            .await
            .unwrap();
        let cache = CacheManager::new(pool);

        // get -> None
        assert_eq!(cache.get("any_key").await, None);

        // mget -> vec of None
        let keys = vec!["k1".to_string(), "k2".to_string()];
        let results = cache.mget(&keys).await;
        assert_eq!(results, vec![None, None]);

        // is_healthy -> false
        assert!(!cache.is_healthy().await);
    }
}
