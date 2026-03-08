use bb8::Pool;
use bb8_redis::RedisConnectionManager;
use metrics::counter;
use moka::future::Cache;
use redis::AsyncCommands;
use std::sync::Arc;
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

/// Two-layer cache manager: moka L1 (in-process, sub-ms) in front of Redis L2.
///
/// At high RPS, the L1 cache eliminates redundant Redis round-trips for hot keys.
/// A 200ms L1 TTL means a key accessed 5000 times/sec hits Redis only ~5 times/sec.
///
/// Every public method returns Option/Result that never propagates Redis errors
/// to the caller. Errors are logged and treated as cache misses.
#[derive(Clone)]
pub struct CacheManager {
    pool: RedisPool,
    /// L1 in-process cache for string values (GET/SET/MGET).
    l1_strings: Cache<String, String>,
    /// L1 in-process cache for sorted set results (ZREVRANGE).
    l1_zsets: Cache<String, Arc<Vec<(String, f64)>>>,
}

impl CacheManager {
    pub fn new(pool: RedisPool, config: &Config) -> Self {
        let l1_ttl = Duration::from_millis(config.local_cache_ttl_ms);
        let l1_max = config.local_cache_max_capacity;

        let l1_strings = Cache::builder()
            .max_capacity(l1_max)
            .time_to_live(l1_ttl)
            .build();

        let l1_zsets = Cache::builder()
            .max_capacity(l1_max / 10) // far fewer sorted set keys
            .time_to_live(l1_ttl)
            .build();

        tracing::info!(
            l1_ttl_ms = config.local_cache_ttl_ms,
            l1_max_capacity = l1_max,
            "CacheManager initialized with moka L1"
        );

        Self {
            pool,
            l1_strings,
            l1_zsets,
        }
    }

    /// Get a string value by key. Checks L1 first, then Redis.
    /// On Redis hit, populates L1 for subsequent requests.
    pub async fn get(&self, key: &str) -> Option<String> {
        // L1 check
        if let Some(val) = self.l1_strings.get(key).await {
            counter!("social_api_cache_operations_total", "operation" => "get", "result" => "l1_hit").increment(1);
            return Some(val);
        }

        // L2 (Redis)
        let mut conn = self.pool.get().await.ok()?;
        match conn.get::<_, Option<String>>(key).await {
            Ok(Some(val)) => {
                counter!("social_api_cache_operations_total", "operation" => "get", "result" => "hit").increment(1);
                self.l1_strings.insert(key.to_string(), val.clone()).await;
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

    /// Set a string value with TTL. Writes to both L1 and Redis.
    pub async fn set(&self, key: &str, value: &str, ttl_secs: u64) {
        // Always update L1 so concurrent readers see fresh data immediately
        self.l1_strings
            .insert(key.to_string(), value.to_string())
            .await;

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

    /// Set multiple string values with TTL. Writes to both L1 and Redis pipeline.
    pub async fn set_many(&self, entries: &[(&str, &str, u64)]) {
        if entries.is_empty() {
            return;
        }

        // Populate L1 immediately
        for (key, value, _ttl) in entries {
            self.l1_strings
                .insert((*key).to_string(), (*value).to_string())
                .await;
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

    /// Delete a key from both L1 and Redis. Silently fails on Redis error.
    #[allow(dead_code)]
    pub async fn del(&self, key: &str) {
        self.l1_strings.remove(key).await;

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

    /// Get multiple keys at once. Checks L1 first, then Redis MGET for misses.
    /// Populates L1 for any Redis hits.
    pub async fn mget(&self, keys: &[String]) -> Vec<Option<String>> {
        if keys.is_empty() {
            return vec![];
        }

        let mut results: Vec<Option<String>> = Vec::with_capacity(keys.len());
        let mut redis_needed: Vec<(usize, &String)> = Vec::new();

        // Phase 1: check L1 for each key
        for (idx, key) in keys.iter().enumerate() {
            if let Some(val) = self.l1_strings.get(key).await {
                counter!("social_api_cache_operations_total", "operation" => "mget", "result" => "l1_hit").increment(1);
                results.push(Some(val));
            } else {
                redis_needed.push((idx, key));
                results.push(None);
            }
        }

        if redis_needed.is_empty() {
            return results;
        }

        // Phase 2: fetch remaining keys from Redis
        let redis_keys: Vec<&String> = redis_needed.iter().map(|(_, k)| *k).collect();
        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                counter!("social_api_cache_operations_total", "operation" => "mget", "result" => "error").increment(1);
                tracing::warn!(error = %e, "Redis pool GET failed for MGET");
                return results; // L1 hits are preserved, rest stays None
            }
        };

        match redis::cmd("MGET")
            .arg(&redis_keys)
            .query_async::<Vec<Option<String>>>(&mut *conn)
            .await
        {
            Ok(vals) => {
                for (i, val) in vals.into_iter().enumerate() {
                    let (result_idx, key) = redis_needed[i];
                    if let Some(ref v) = val {
                        counter!("social_api_cache_operations_total", "operation" => "mget", "result" => "hit").increment(1);
                        self.l1_strings.insert(key.to_string(), v.clone()).await;
                    } else {
                        counter!("social_api_cache_operations_total", "operation" => "mget", "result" => "miss").increment(1);
                    }
                    results[result_idx] = val;
                }
            }
            Err(e) => {
                counter!("social_api_cache_operations_total", "operation" => "mget", "result" => "error").increment(1);
                tracing::warn!(error = %e, "Redis MGET failed");
            }
        }

        results
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
    /// Also invalidates all L1 zset cache entries for this key (any range).
    pub async fn replace_sorted_set(&self, key: &str, members: &[(String, f64)]) {
        // Invalidate all L1 entries whose key starts with "{key}:" (any range variant)
        let prefix = format!("{key}:");
        self.l1_zsets
            .invalidate_entries_if(move |k, _v| k.starts_with(&prefix))
            .ok();

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
    /// Checks L1 first, then Redis. Populates L1 on Redis hit.
    ///
    /// L1 key is `"{key}:{start}:{stop}"` to distinguish different range queries.
    pub async fn zrevrange_with_scores(
        &self,
        key: &str,
        start: isize,
        stop: isize,
    ) -> Vec<(String, f64)> {
        let l1_key = format!("{key}:{start}:{stop}");

        // L1 check
        if let Some(cached) = self.l1_zsets.get(&l1_key).await {
            counter!("social_api_cache_operations_total", "operation" => "zrevrange", "result" => "l1_hit").increment(1);
            return (*cached).clone();
        }

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
            Ok(pairs) => {
                if !pairs.is_empty() {
                    self.l1_zsets.insert(l1_key, Arc::new(pairs.clone())).await;
                }
                pairs
            }
            Err(e) => {
                tracing::warn!(key = key, error = %e, "Redis ZREVRANGE failed");
                vec![]
            }
        }
    }

    /// Delete multiple keys from both L1 and Redis in a pipeline.
    ///
    /// Removes matching entries from both the string and sorted-set L1 caches.
    /// The sorted-set L1 uses `"{key}:{start}:{stop}"` compound keys, so prefix
    /// invalidation is used to catch all range variants of a given base key.
    pub async fn del_many(&self, keys: &[&str]) {
        if keys.is_empty() {
            return;
        }

        for key in keys {
            self.l1_strings.remove(*key).await;
            // Sorted-set L1 keys are stored as "{key}:{start}:{stop}". Invalidate
            // all range variants so stale ZSET data is never served after deletion.
            let prefix = format!("{key}:");
            self.l1_zsets
                .invalidate_entries_if(move |k, _v| k.starts_with(&prefix))
                .ok();
        }

        let mut conn = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Redis pool GET failed for del_many");
                return;
            }
        };
        let mut pipe = redis::pipe();
        for key in keys {
            pipe.del(*key).ignore();
        }
        if let Err(e) = pipe.query_async::<()>(&mut *conn).await {
            tracing::warn!(error = %e, "Redis del_many pipeline failed");
        }
    }

    /// Invalidate all L1 string cache entries whose key starts with `prefix`.
    /// Does NOT touch Redis — callers handle Redis invalidation separately
    /// (via del_many for explicit keys or TTL expiry for natural eviction).
    pub fn invalidate_l1_prefix(&self, prefix: &str) {
        let prefix = prefix.to_string();
        self.l1_strings
            .invalidate_entries_if(move |k, _v| k.starts_with(&prefix))
            .ok();
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

    struct TestHarness {
        _scope: crate::test_containers::TestScope,
        cache: CacheManager,
    }

    /// Return a CacheManager connected to the shared Redis container.
    async fn make_cache() -> TestHarness {
        let scope = crate::test_containers::isolated_scope().await;
        let mut config = crate::config::Config::new_for_test();
        config.redis_url = scope.redis_url.clone();
        let pool = create_pool(&config).await.unwrap();
        let cache = CacheManager::new(pool, &config);
        TestHarness {
            _scope: scope,
            cache,
        }
    }

    // -------------------------------------------------------------------------
    // 1. get / set round-trip
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_set() {
        let harness = make_cache().await;
        let cache = harness.cache;
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
        let harness = make_cache().await;
        let cache = harness.cache;
        assert_eq!(cache.get("test:definitely_absent_key").await, None);
    }

    // -------------------------------------------------------------------------
    // 3. mget returns hits, misses, and empty input
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_mget() {
        let harness = make_cache().await;
        let cache = harness.cache;

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
        let harness = make_cache().await;
        let cache = harness.cache;
        let results = cache.mget(&[]).await;
        assert!(results.is_empty());
    }

    // -------------------------------------------------------------------------
    // 4. replace_sorted_set then zrevrange_with_scores returns descending order
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_zrevrange_with_scores() {
        let harness = make_cache().await;
        let cache = harness.cache;
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
        let harness = make_cache().await;
        let cache = harness.cache;
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
        let harness = make_cache().await;
        let cache = harness.cache;
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
        let config = crate::config::Config::new_for_test();
        let cache = CacheManager::new(pool, &config);

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
