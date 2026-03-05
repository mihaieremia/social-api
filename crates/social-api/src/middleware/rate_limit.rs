use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use shared::errors::{ApiError, ErrorCode};
use std::sync::LazyLock;

use crate::cache::manager::CacheManager;
use crate::state::AppState;

/// Lua script for atomic sliding window rate limiting.
/// Operations: ZREMRANGEBYSCORE (prune old), ZCARD (count), ZADD (add current).
/// Returns {allowed, count, limit, reset_ms} for accurate Retry-After headers.
/// Uses redis::Script for automatic EVALSHA caching.
static RATE_LIMIT_SCRIPT: LazyLock<redis::Script> = LazyLock::new(|| {
    redis::Script::new(
        r#"
local key = KEYS[1]
local now = tonumber(ARGV[1])
local window = tonumber(ARGV[2])
local limit = tonumber(ARGV[3])
local member = ARGV[4]

-- Remove entries outside the window
redis.call('ZREMRANGEBYSCORE', key, 0, now - window)

-- Count current entries
local count = redis.call('ZCARD', key)

if count >= limit then
    -- Calculate actual reset time from oldest entry
    local oldest = redis.call('ZRANGE', key, 0, 0, 'WITHSCORES')
    local reset_ms = 0
    if #oldest >= 2 then
        reset_ms = tonumber(oldest[2]) + window - now
        if reset_ms < 0 then reset_ms = 0 end
    end
    return {0, count, limit, reset_ms}
end

-- Add current request
redis.call('ZADD', key, now, member)
redis.call('EXPIRE', key, math.ceil(window / 1000) + 10)

-- Calculate reset from oldest entry after add
local oldest = redis.call('ZRANGE', key, 0, 0, 'WITHSCORES')
local reset_ms = window
if #oldest >= 2 then
    reset_ms = tonumber(oldest[2]) + window - now
    if reset_ms < 0 then reset_ms = 0 end
end

return {1, count + 1, limit, reset_ms}
"#,
    )
});

/// Rate limit result.
struct RateLimitResult {
    allowed: bool,
    current: i64,
    limit: i64,
    reset_secs: u64,
}

/// Check rate limit using Redis sliding window.
async fn check_rate_limit_inner(
    cache: &CacheManager,
    key: &str,
    limit: u64,
    window_secs: u64,
) -> RateLimitResult {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string();

    let window_ms = (window_secs * 1000).to_string();
    let limit_str = limit.to_string();
    let member = format!("{}:{}", now, uuid::Uuid::new_v4());

    let result = cache
        .invoke_script(
            &RATE_LIMIT_SCRIPT,
            &[key],
            &[&now, &window_ms, &limit_str, &member],
        )
        .await;

    match result {
        Some(redis::Value::Array(ref values)) if values.len() >= 3 => {
            let allowed = extract_int(&values[0]).unwrap_or(1) == 1;
            let current = extract_int(&values[1]).unwrap_or(0);
            let max = extract_int(&values[2]).unwrap_or(limit as i64);

            // Use actual reset time if available (4th value = ms until oldest expires)
            let reset_secs = if values.len() >= 4 {
                let reset_ms = extract_int(&values[3]).unwrap_or((window_secs * 1000) as i64);
                // Convert ms to seconds, ceiling division, clamp to [1, window_secs]
                ((reset_ms as u64).saturating_add(999) / 1000).clamp(1, window_secs)
            } else {
                window_secs
            };

            RateLimitResult {
                allowed,
                current,
                limit: max,
                reset_secs,
            }
        }
        _ => {
            // Redis unavailable — fail open (allow the request).
            // Intentional: degraded performance (no rate limiting) is preferable to
            // dropping legitimate traffic when the cache is down.
            // Monitor `social_api_cache_operations_total{result="error"}` to alert on
            // Redis unavailability and restore rate limiting as soon as Redis recovers.
            RateLimitResult {
                allowed: true,
                current: 0,
                limit: limit as i64,
                reset_secs: window_secs,
            }
        }
    }
}

fn extract_int(value: &redis::Value) -> Option<i64> {
    match value {
        redis::Value::Int(n) => Some(*n),
        _ => None,
    }
}

/// Add rate limit headers to response.
fn add_rate_limit_headers(response: &mut Response, result: &RateLimitResult) {
    let remaining = (result.limit - result.current).max(0);

    let headers = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&result.limit.to_string()) {
        headers.insert("X-RateLimit-Limit", v);
    }
    if let Ok(v) = HeaderValue::from_str(&remaining.to_string()) {
        headers.insert("X-RateLimit-Remaining", v);
    }
    if let Ok(v) = HeaderValue::from_str(&result.reset_secs.to_string()) {
        headers.insert("X-RateLimit-Reset", v);
    }
}

/// Build a 429 response with rate limit headers.
fn rate_limited_response(result: &RateLimitResult) -> Response {
    let api_error = ApiError::new(ErrorCode::RateLimited, "Rate limit exceeded", "unknown");
    let mut response = (StatusCode::TOO_MANY_REQUESTS, Json(api_error)).into_response();

    add_rate_limit_headers(&mut response, result);
    if let Ok(v) = HeaderValue::from_str(&result.reset_secs.to_string()) {
        response.headers_mut().insert("Retry-After", v);
    }
    response
}

/// Write rate limit middleware (per-user, applied to POST/DELETE).
/// Uses `from_fn_with_state` to access AppState.
pub async fn write_rate_limit(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let user_token = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let key = format!("rl:w:{}", fnv1a_hash(&user_token));
    let limit = state.config().rate_limit_write_per_minute;
    let result = check_rate_limit_inner(state.cache(), &key, limit, 60).await;

    if !result.allowed {
        return rate_limited_response(&result);
    }

    let mut response = next.run(request).await;
    add_rate_limit_headers(&mut response, &result);
    response
}

/// Extract the real client IP for rate limiting.
///
/// Uses the RIGHTMOST value in X-Forwarded-For — the address appended by the
/// last trusted proxy (load balancer), which clients cannot forge. Falls back
/// to X-Real-IP (set by nginx/envoy trusted proxies), then "unknown".
fn extract_real_ip(header_value: &str) -> &str {
    header_value
        .rsplit(',')
        .next()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown")
}

/// Read rate limit middleware (per-IP, applied to GET).
/// Uses `from_fn_with_state` to access AppState.
pub async fn read_rate_limit(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let ip = {
        let raw = request
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .or_else(|| {
                request
                    .headers()
                    .get("x-real-ip")
                    .and_then(|v| v.to_str().ok())
            })
            .unwrap_or("unknown");
        extract_real_ip(raw).to_string()
    };

    let key = format!("rl:r:{}", fnv1a_hash(&ip));
    let limit = state.config().rate_limit_read_per_minute;
    let result = check_rate_limit_inner(state.cache(), &key, limit, 60).await;

    if !result.allowed {
        return rate_limited_response(&result);
    }

    let mut response = next.run(request).await;
    add_rate_limit_headers(&mut response, &result);
    response
}

/// Deterministic FNV-1a hash for rate limit keys.
/// Unlike DefaultHasher (SipHash with random seeds), FNV-1a produces identical
/// hashes across all replicas, ensuring correct per-user/per-IP rate limiting.
fn fnv1a_hash(input: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x00000100000001B3;
    let mut hash = FNV_OFFSET;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use testcontainers::runners::AsyncRunner;

    // --- Pure function tests (no infrastructure needed) ---

    #[test]
    fn test_extract_rightmost_ip_from_forwarded_for() {
        assert_eq!(
            extract_real_ip("1.1.1.1, 2.2.2.2, 10.0.0.1"),
            "10.0.0.1"
        );
        assert_eq!(extract_real_ip("192.168.1.100"), "192.168.1.100");
        assert_eq!(extract_real_ip("  1.2.3.4  ,  5.6.7.8  "), "5.6.7.8");
    }

    #[test]
    fn test_fnv1a_hash_is_deterministic() {
        assert_eq!(fnv1a_hash("tok_user_1"), fnv1a_hash("tok_user_1"));
        assert_ne!(fnv1a_hash("tok_user_1"), fnv1a_hash("tok_user_2"));
    }

    #[test]
    fn test_fnv1a_hash_different_inputs_differ() {
        let inputs = ["", "a", "ab", "abc", "192.168.1.1", "10.0.0.1"];
        let hashes: Vec<u64> = inputs.iter().map(|s| fnv1a_hash(s)).collect();
        let unique: std::collections::HashSet<u64> = hashes.iter().copied().collect();
        assert_eq!(unique.len(), inputs.len());
    }

    #[test]
    fn test_add_rate_limit_headers_sets_correct_values() {
        let result = RateLimitResult {
            allowed: true,
            current: 5,
            limit: 30,
            reset_secs: 45,
        };
        let mut response = axum::response::Response::new(axum::body::Body::empty());
        add_rate_limit_headers(&mut response, &result);

        let headers = response.headers();
        assert_eq!(headers.get("X-RateLimit-Limit").unwrap(), "30");
        assert_eq!(headers.get("X-RateLimit-Remaining").unwrap(), "25");
        assert_eq!(headers.get("X-RateLimit-Reset").unwrap(), "45");
    }

    #[test]
    fn test_rate_limited_response_is_429() {
        let result = RateLimitResult {
            allowed: false,
            current: 30,
            limit: 30,
            reset_secs: 12,
        };
        let response = rate_limited_response(&result);
        assert_eq!(response.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().get("Retry-After").is_some());
    }

    // --- Redis Lua script tests (testcontainers) ---

    #[tokio::test]
    async fn test_sliding_window_allows_under_limit() {
        let redis = testcontainers_modules::redis::Redis::default()
            .start()
            .await
            .expect("redis container");
        let port = redis.get_host_port_ipv4(6379).await.unwrap();

        let mut config = crate::config::Config::new_for_test();
        config.redis_url = format!("redis://127.0.0.1:{port}");
        let pool = crate::cache::manager::create_pool(&config).await.unwrap();
        let cache = CacheManager::new(pool);

        let result = check_rate_limit_inner(&cache, "rl:test:allow", 10, 60).await;
        assert!(result.allowed);
        assert_eq!(result.current, 1);
        assert_eq!(result.limit, 10);
    }

    #[tokio::test]
    async fn test_sliding_window_blocks_over_limit() {
        let redis = testcontainers_modules::redis::Redis::default()
            .start()
            .await
            .expect("redis container");
        let port = redis.get_host_port_ipv4(6379).await.unwrap();

        let mut config = crate::config::Config::new_for_test();
        config.redis_url = format!("redis://127.0.0.1:{port}");
        let pool = crate::cache::manager::create_pool(&config).await.unwrap();
        let cache = CacheManager::new(pool);

        // Exhaust the limit of 3
        for _ in 0..3 {
            check_rate_limit_inner(&cache, "rl:test:block", 3, 60).await;
        }

        let result = check_rate_limit_inner(&cache, "rl:test:block", 3, 60).await;
        assert!(!result.allowed);
        assert_eq!(result.current, 3);
    }

    #[tokio::test]
    async fn test_sliding_window_fails_open_on_redis_unavailable() {
        // Point at a port with no Redis — cache ops degrade gracefully
        let mut config = crate::config::Config::new_for_test();
        config.redis_url = "redis://127.0.0.1:19999".to_string();
        // bb8 pool with fast timeout so the test doesn't hang
        let manager =
            bb8_redis::RedisConnectionManager::new(config.redis_url.as_str()).unwrap();
        let pool = bb8::Pool::builder()
            .connection_timeout(std::time::Duration::from_millis(100))
            .build(manager)
            .await
            .unwrap();
        let cache = CacheManager::new(pool);

        let result = check_rate_limit_inner(&cache, "rl:test:failopen", 10, 60).await;
        // Must fail open (allow) — never drop legitimate traffic when Redis is down
        assert!(result.allowed);
    }
}
