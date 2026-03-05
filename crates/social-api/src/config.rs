use std::collections::HashMap;
use std::env;

/// Application configuration loaded from environment variables.
/// Fails fast on startup if required variables are missing.
#[derive(Debug, Clone)]
pub struct Config {
    // Required
    pub database_url: String,
    pub read_database_url: String,
    pub redis_url: String,
    pub http_port: u16,

    // Content API URLs (keyed by content type)
    pub content_api_urls: HashMap<String, String>,
    pub profile_api_url: String,

    // Database pool
    pub db_max_connections: u32,
    pub db_min_connections: u32,
    pub db_acquire_timeout_secs: u64,

    // Redis pool
    pub redis_pool_size: u32,

    // Rate limiting
    pub rate_limit_write_per_minute: u64,
    pub rate_limit_read_per_minute: u64,

    // Cache TTLs
    pub cache_ttl_like_counts_secs: u64,
    pub cache_ttl_content_validation_secs: u64,
    /// Token validation cache TTL. Used by HttpTokenValidator to cache
    /// authenticated user data in Redis, reducing Profile API round-trips.
    pub cache_ttl_user_status_secs: u64,

    // Circuit breaker
    pub circuit_breaker_failure_threshold: u32,
    pub circuit_breaker_recovery_timeout_secs: u64,
    pub circuit_breaker_success_threshold: u32,
    /// Sliding window duration for circuit breaker failure-rate calculation.
    /// Separate from recovery_timeout to allow independent tuning.
    /// Default: 30s (spec recommendation).
    pub circuit_breaker_rate_window_secs: u64,

    // Shutdown
    pub shutdown_timeout_secs: u64,

    // SSE
    pub sse_heartbeat_interval_secs: u64,
    pub sse_broadcast_capacity: usize,

    // Leaderboard
    pub leaderboard_refresh_interval_secs: u64,

    // Logging
    pub log_level: String,
}

impl Config {
    /// Load configuration from environment variables.
    /// Panics with a descriptive message if required variables are missing.
    pub fn from_env() -> Self {
        let mut missing: Vec<&str> = Vec::new();

        let database_url = require_env("DATABASE_URL", &mut missing);
        let read_database_url = require_env("READ_DATABASE_URL", &mut missing);
        let redis_url = require_env("REDIS_URL", &mut missing);
        let http_port_str = require_env("HTTP_PORT", &mut missing);
        let profile_api_url = require_env("PROFILE_API_URL", &mut missing);

        if !missing.is_empty() {
            panic!(
                "Missing required environment variables: {}",
                missing.join(", ")
            );
        }

        let http_port: u16 = http_port_str.parse().unwrap_or_else(|_| {
            panic!("HTTP_PORT must be a valid port number, got: {http_port_str}")
        });

        // Build content API URL map from CONTENT_API_{TYPE}_URL env vars
        let content_api_urls = build_content_api_urls();

        if content_api_urls.is_empty() {
            panic!(
                "No content API URLs configured. Set at least one CONTENT_API_{{TYPE}}_URL env var (e.g., CONTENT_API_POST_URL)"
            );
        }

        Self {
            database_url,
            read_database_url,
            redis_url,
            http_port,
            content_api_urls,
            profile_api_url,
            db_max_connections: env_or_default("DB_MAX_CONNECTIONS", 20),
            db_min_connections: env_or_default("DB_MIN_CONNECTIONS", 5),
            db_acquire_timeout_secs: env_or_default("DB_ACQUIRE_TIMEOUT_SECS", 5),
            redis_pool_size: env_or_default("REDIS_POOL_SIZE", 10),
            rate_limit_write_per_minute: env_or_default("RATE_LIMIT_WRITE_PER_MINUTE", 30),
            rate_limit_read_per_minute: env_or_default("RATE_LIMIT_READ_PER_MINUTE", 1000),
            cache_ttl_like_counts_secs: env_or_default("CACHE_TTL_LIKE_COUNTS_SECS", 300),
            cache_ttl_content_validation_secs: env_or_default(
                "CACHE_TTL_CONTENT_VALIDATION_SECS",
                3600,
            ),
            cache_ttl_user_status_secs: env_or_default("CACHE_TTL_USER_STATUS_SECS", 60),
            circuit_breaker_failure_threshold: env_or_default(
                "CIRCUIT_BREAKER_FAILURE_THRESHOLD",
                5,
            ),
            circuit_breaker_recovery_timeout_secs: env_or_default(
                "CIRCUIT_BREAKER_RECOVERY_TIMEOUT_SECS",
                30,
            ),
            circuit_breaker_success_threshold: env_or_default(
                "CIRCUIT_BREAKER_SUCCESS_THRESHOLD",
                3,
            ),
            circuit_breaker_rate_window_secs: env_or_default("CIRCUIT_BREAKER_RATE_WINDOW_SECS", 30),
            shutdown_timeout_secs: env_or_default("SHUTDOWN_TIMEOUT_SECS", 30),
            sse_heartbeat_interval_secs: env_or_default("SSE_HEARTBEAT_INTERVAL_SECS", 15),
            sse_broadcast_capacity: env_or_default("SSE_BROADCAST_CAPACITY", 256),
            leaderboard_refresh_interval_secs: env_or_default(
                "LEADERBOARD_REFRESH_INTERVAL_SECS",
                60,
            ),
            log_level: env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string()),
        }
    }

    /// Check if a content type is registered.
    pub fn is_valid_content_type(&self, content_type: &str) -> bool {
        self.content_api_urls.contains_key(content_type)
    }

    /// Get the API URL for a content type.
    pub fn content_api_url(&self, content_type: &str) -> Option<&str> {
        self.content_api_urls.get(content_type).map(|s| s.as_str())
    }

    /// Minimal config for unit tests — does NOT read environment variables.
    #[cfg(test)]
    pub fn new_for_test() -> Self {
        let mut content_api_urls = std::collections::HashMap::new();
        content_api_urls.insert("post".to_string(), "http://localhost:8081".to_string());
        content_api_urls.insert(
            "bonus_hunter".to_string(),
            "http://localhost:8081".to_string(),
        );
        content_api_urls.insert(
            "top_picks".to_string(),
            "http://localhost:8081".to_string(),
        );

        Self {
            http_port: 8080,
            database_url: "postgres://social:social_password@localhost:5432/social_api_test"
                .to_string(),
            read_database_url:
                "postgres://social:social_password@localhost:5432/social_api_test".to_string(),
            redis_url: "redis://localhost:6379".to_string(),
            content_api_urls,
            profile_api_url: "http://localhost:8081".to_string(),
            db_max_connections: 5,
            db_min_connections: 1,
            db_acquire_timeout_secs: 5,
            redis_pool_size: 5,
            rate_limit_write_per_minute: 30,
            rate_limit_read_per_minute: 1000,
            cache_ttl_like_counts_secs: 300,
            cache_ttl_content_validation_secs: 3600,
            cache_ttl_user_status_secs: 60,
            circuit_breaker_failure_threshold: 5,
            circuit_breaker_recovery_timeout_secs: 30,
            circuit_breaker_success_threshold: 2,
            circuit_breaker_rate_window_secs: 30,
            leaderboard_refresh_interval_secs: 300,
            shutdown_timeout_secs: 30,
            sse_heartbeat_interval_secs: 15,
            sse_broadcast_capacity: 128,
            log_level: "info".to_string(),
        }
    }
}

/// Require an environment variable, recording its name if missing.
fn require_env<'a>(name: &'a str, missing: &mut Vec<&'a str>) -> String {
    match env::var(name) {
        Ok(val) if !val.is_empty() => val,
        _ => {
            missing.push(name);
            String::new()
        }
    }
}

/// Get an environment variable with a default value.
fn env_or_default<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Scan environment for CONTENT_API_{TYPE}_URL variables.
/// Converts the TYPE portion to lowercase with underscores.
///
/// Example: CONTENT_API_POST_URL -> ("post", "http://...")
///          CONTENT_API_BONUS_HUNTER_URL -> ("bonus_hunter", "http://...")
fn build_content_api_urls() -> HashMap<String, String> {
    let mut urls = HashMap::new();

    // Known content types from spec
    let known_types = [
        ("CONTENT_API_POST_URL", "post"),
        ("CONTENT_API_BONUS_HUNTER_URL", "bonus_hunter"),
        ("CONTENT_API_TOP_PICKS_URL", "top_picks"),
    ];

    for (env_var, content_type) in &known_types {
        if let Ok(url) = env::var(env_var)
            && !url.is_empty()
        {
            urls.insert(content_type.to_string(), url);
        }
    }

    // Also scan for any additional CONTENT_API_*_URL patterns
    for (key, value) in env::vars() {
        if key.starts_with("CONTENT_API_")
            && key.ends_with("_URL")
            && !known_types.iter().any(|(k, _)| *k == key)
        {
            // Extract type name: CONTENT_API_NEWS_ARTICLE_URL -> news_article
            let type_name = key
                .strip_prefix("CONTENT_API_")
                .and_then(|s| s.strip_suffix("_URL"))
                .map(|s| s.to_lowercase());

            if let Some(type_name) = type_name
                && !value.is_empty()
            {
                urls.insert(type_name, value);
            }
        }
    }

    urls
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_env_or_default_with_missing_var() {
        // Use a unique name unlikely to exist
        let result: u32 = env_or_default("__TEST_MISSING_VAR_12345__", 42);
        assert_eq!(result, 42);
    }

    #[test]
    fn test_env_or_default_with_invalid_value() {
        // SAFETY: Single-threaded test, no concurrent env access
        unsafe { env::set_var("__TEST_INVALID_VAR__", "not_a_number") };
        let result: u32 = env_or_default("__TEST_INVALID_VAR__", 42);
        assert_eq!(result, 42);
        unsafe { env::remove_var("__TEST_INVALID_VAR__") };
    }
}
