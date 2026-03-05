use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use shared::errors::{ApiError, ErrorCode};

use crate::cache::manager::CacheManager;
use crate::state::AppState;

/// Lua script for atomic sliding window rate limiting.
/// Operations: ZREMRANGEBYSCORE (prune old), ZCARD (count), ZADD (add current).
/// All in a single EVAL for atomicity.
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

    let key = format!("rl:w:{}", simple_hash(&user_token));
    let limit = state.config().rate_limit_write_per_minute;
    let result = check_rate_limit(state.cache(), &key, limit, 60).await;

    if !result.allowed {
        return rate_limited_response(&result);
    }

    let mut response = next.run(request).await;
    add_rate_limit_headers(&mut response, &result);
    response
}

/// Read rate limit middleware (per-IP, applied to GET).
/// Uses `from_fn_with_state` to access AppState.
pub async fn read_rate_limit(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let ip = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            request
                .headers()
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
        })
        .unwrap_or("unknown")
        .to_string();

    let key = format!("rl:r:{}", simple_hash(&ip));
    let limit = state.config().rate_limit_read_per_minute;
    let result = check_rate_limit(state.cache(), &key, limit, 60).await;

    if !result.allowed {
        return rate_limited_response(&result);
    }

    let mut response = next.run(request).await;
    add_rate_limit_headers(&mut response, &result);
    response
}

/// Simple hash for rate limit keys (avoids storing raw tokens/IPs).
fn simple_hash(input: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    input.hash(&mut hasher);
    hasher.finish()
}
