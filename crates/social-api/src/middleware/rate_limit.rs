use axum::{
    extract::Request,
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use shared::errors::{ApiError, ErrorCode};

use crate::cache::manager::CacheManager;

/// Lua script for atomic sliding window rate limiting.
/// Operations: ZREMRANGEBYSCORE (prune old), ZCARD (count), ZADD (add current).
/// All in a single EVAL for atomicity across replicas.
const RATE_LIMIT_SCRIPT: &str = r#"
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
    return {0, count, limit}
end

-- Add current request
redis.call('ZADD', key, now, member)
redis.call('EXPIRE', key, window + 10)

return {1, count + 1, limit}
"#;

/// Rate limit result.
struct RateLimitResult {
    allowed: bool,
    current: i64,
    limit: i64,
    reset_secs: u64,
}

/// Check rate limit using Redis sliding window.
async fn check_rate_limit(
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
        .eval_script(
            RATE_LIMIT_SCRIPT,
            &[key],
            &[&now, &window_ms, &limit_str, &member],
        )
        .await;

    match result {
        Some(redis::Value::Array(ref values)) if values.len() == 3 => {
            let allowed = extract_int(&values[0]).unwrap_or(1) == 1;
            let current = extract_int(&values[1]).unwrap_or(0);
            let max = extract_int(&values[2]).unwrap_or(limit as i64);

            RateLimitResult {
                allowed,
                current,
                limit: max,
                reset_secs: window_secs,
            }
        }
        _ => {
            // Redis unavailable — allow request (graceful degradation)
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

/// Write rate limit middleware (per-user, 30/min default).
/// Applied to POST/DELETE endpoints that require authentication.
pub async fn write_rate_limit(request: Request, next: Next) -> Response {
    // Extract user_id from auth header to build rate limit key
    let user_token = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Get cache and config from extensions
    let cache = request.extensions().get::<CacheManager>().cloned();
    let write_limit = request
        .extensions()
        .get::<RateLimitConfig>()
        .map(|c| c.write_per_minute)
        .unwrap_or(30);

    if let Some(cache) = cache {
        let key = format!("rl:w:{}", simple_hash(&user_token));
        let result = check_rate_limit(&cache, &key, write_limit, 60).await;

        if !result.allowed {
            let api_error = ApiError::new(
                ErrorCode::RateLimited,
                "Rate limit exceeded",
                "unknown",
            );
            let mut response = (
                StatusCode::TOO_MANY_REQUESTS,
                Json(api_error),
            )
                .into_response();

            add_rate_limit_headers(&mut response, &result);
            if let Ok(v) = HeaderValue::from_str(&result.reset_secs.to_string()) {
                response.headers_mut().insert("Retry-After", v);
            }
            return response;
        }

        let mut response = next.run(request).await;
        add_rate_limit_headers(&mut response, &result);
        response
    } else {
        // No cache available — allow (graceful degradation)
        next.run(request).await
    }
}

/// Read rate limit middleware (per-IP, 1000/min default).
pub async fn read_rate_limit(request: Request, next: Next) -> Response {
    let ip = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            request
                .extensions()
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|ci| "unknown") // Simplified for now
        })
        .unwrap_or("unknown")
        .to_string();

    let cache = request.extensions().get::<CacheManager>().cloned();
    let read_limit = request
        .extensions()
        .get::<RateLimitConfig>()
        .map(|c| c.read_per_minute)
        .unwrap_or(1000);

    if let Some(cache) = cache {
        let key = format!("rl:r:{}", simple_hash(&ip));
        let result = check_rate_limit(&cache, &key, read_limit, 60).await;

        if !result.allowed {
            let api_error = ApiError::new(
                ErrorCode::RateLimited,
                "Rate limit exceeded",
                "unknown",
            );
            let mut response = (
                StatusCode::TOO_MANY_REQUESTS,
                Json(api_error),
            )
                .into_response();

            add_rate_limit_headers(&mut response, &result);
            if let Ok(v) = HeaderValue::from_str(&result.reset_secs.to_string()) {
                response.headers_mut().insert("Retry-After", v);
            }
            return response;
        }

        let mut response = next.run(request).await;
        add_rate_limit_headers(&mut response, &result);
        response
    } else {
        next.run(request).await
    }
}

/// Simple hash for rate limit keys (avoids storing raw tokens/IPs).
fn simple_hash(input: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    input.hash(&mut hasher);
    hasher.finish()
}

/// Rate limit configuration injected via request extensions.
#[derive(Clone, Debug)]
pub struct RateLimitConfig {
    pub write_per_minute: u64,
    pub read_per_minute: u64,
}
