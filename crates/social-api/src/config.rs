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
    #[allow(dead_code)]
    pub grpc_port: u16,
    /// Transport for outgoing inter-service calls: "http" or "grpc"
    #[allow(dead_code)]
    pub internal_transport: String,

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

    // Server concurrency
    /// Maximum number of concurrent in-flight requests the server will accept.
    /// Requests beyond this limit receive 503 Service Unavailable immediately,
    /// providing backpressure instead of queuing until timeout.
    pub server_concurrency_limit: usize,

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

        let grpc_port: u16 = env_or_default("GRPC_PORT", 50051);
        let internal_transport =
            env::var("INTERNAL_TRANSPORT").unwrap_or_else(|_| "http".to_string());

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
            grpc_port,
            internal_transport,
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
            circuit_breaker_rate_window_secs: env_or_default(
                "CIRCUIT_BREAKER_RATE_WINDOW_SECS",
                30,
            ),
            server_concurrency_limit: env_or_default("SERVER_CONCURRENCY_LIMIT", 10000),
            shutdown_timeout_secs: env_or_default("SHUTDOWN_TIMEOUT_SECS", 30),
            sse_heartbeat_interval_secs: env_or_default("SSE_HEARTBEAT_INTERVAL_SECS", 15),
            sse_broadcast_capacity: env_or_default("SSE_BROADCAST_CAPACITY", 128),
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

    /// Check if gRPC transport is configured for internal calls.
    #[allow(dead_code)]
    pub fn use_grpc_transport(&self) -> bool {
        self.internal_transport.eq_ignore_ascii_case("grpc")
    }

    /// Minimal config for unit/integration tests — does NOT read environment variables.
    #[allow(dead_code)]
    pub fn new_for_test() -> Self {
        let mut content_api_urls = std::collections::HashMap::new();
        content_api_urls.insert("post".to_string(), "http://localhost:8081".to_string());
        content_api_urls.insert(
            "bonus_hunter".to_string(),
            "http://localhost:8081".to_string(),
        );
        content_api_urls.insert("top_picks".to_string(), "http://localhost:8081".to_string());

        Self {
            http_port: 8080,
            grpc_port: 50051,
            internal_transport: "http".to_string(),
            database_url: "postgres://social:social_password@localhost:5432/social_api_test"
                .to_string(),
            read_database_url: "postgres://social:social_password@localhost:5432/social_api_test"
                .to_string(),
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
            server_concurrency_limit: 10000,
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
/// Extracts the TYPE portion, strips the prefix/suffix, and lowercases it.
///
/// Example: CONTENT_API_POST_URL -> ("post", "http://...")
///          CONTENT_API_BONUS_HUNTER_URL -> ("bonus_hunter", "http://...")
fn build_content_api_urls() -> HashMap<String, String> {
    let mut urls = HashMap::new();

    for (key, value) in env::vars() {
        if let Some(type_name) = key
            .strip_prefix("CONTENT_API_")
            .and_then(|s| s.strip_suffix("_URL"))
            && !value.is_empty()
        {
            urls.insert(type_name.to_lowercase(), value);
        }
    }

    urls
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn test_env_or_default_with_missing_var() {
        // Use a unique name unlikely to exist
        let result: u32 = env_or_default("__TEST_MISSING_VAR_12345__", 42);
        assert_eq!(result, 42);
    }

    #[test]
    #[serial]
    fn test_env_or_default_with_invalid_value() {
        // SAFETY: Single-threaded test, no concurrent env access
        unsafe { env::set_var("__TEST_INVALID_VAR__", "not_a_number") };
        let result: u32 = env_or_default("__TEST_INVALID_VAR__", 42);
        assert_eq!(result, 42);
        unsafe { env::remove_var("__TEST_INVALID_VAR__") };
    }

    #[test]
    fn test_is_valid_content_type_known() {
        let config = Config::new_for_test();
        assert!(config.is_valid_content_type("post"));
        assert!(config.is_valid_content_type("bonus_hunter"));
        assert!(config.is_valid_content_type("top_picks"));
    }

    #[test]
    fn test_is_valid_content_type_unknown() {
        let config = Config::new_for_test();
        assert!(!config.is_valid_content_type("nonexistent_xyz"));
    }

    #[test]
    fn test_content_api_url_returns_url_for_known_type() {
        let config = Config::new_for_test();
        assert!(config.content_api_url("post").is_some());
        assert_eq!(
            config.content_api_url("post"),
            Some("http://localhost:8081")
        );
    }

    #[test]
    fn test_content_api_url_returns_none_for_unknown() {
        let config = Config::new_for_test();
        assert!(config.content_api_url("totally_unknown_type").is_none());
    }

    #[test]
    #[serial]
    fn test_env_or_default_with_valid_value() {
        unsafe { std::env::set_var("__TEST_VALID_U32_COVERAGE__", "99") };
        let result: u32 = env_or_default("__TEST_VALID_U32_COVERAGE__", 0);
        assert_eq!(result, 99);
        unsafe { std::env::remove_var("__TEST_VALID_U32_COVERAGE__") };
    }

    #[test]
    fn test_new_for_test_has_all_content_types() {
        let config = Config::new_for_test();
        assert!(config.content_api_urls.contains_key("post"));
        assert!(config.content_api_urls.contains_key("bonus_hunter"));
        assert!(config.content_api_urls.contains_key("top_picks"));
        assert_eq!(config.content_api_urls.len(), 3);
    }

    #[test]
    fn test_new_for_test_defaults() {
        let config = Config::new_for_test();
        assert_eq!(config.http_port, 8080);
        assert_eq!(config.grpc_port, 50051);
        assert_eq!(config.internal_transport, "http");
        assert!(!config.use_grpc_transport());
        assert_eq!(config.rate_limit_write_per_minute, 30);
        assert_eq!(config.rate_limit_read_per_minute, 1000);
    }

    #[test]
    fn test_require_env_returns_empty_and_records_missing() {
        let mut missing: Vec<&str> = Vec::new();
        let val = require_env("__MISSING_VAR_COVERAGE_TEST__", &mut missing);
        assert_eq!(val, "");
        assert_eq!(missing, vec!["__MISSING_VAR_COVERAGE_TEST__"]);
    }

    #[test]
    #[serial]
    fn test_require_env_with_empty_string_value() {
        unsafe { env::set_var("__EMPTY_VAR_COVERAGE_TEST__", "") };
        let mut missing: Vec<&str> = Vec::new();
        let val = require_env("__EMPTY_VAR_COVERAGE_TEST__", &mut missing);
        assert_eq!(val, "");
        assert_eq!(missing, vec!["__EMPTY_VAR_COVERAGE_TEST__"]);
        unsafe { env::remove_var("__EMPTY_VAR_COVERAGE_TEST__") };
    }

    #[test]
    #[serial]
    fn test_require_env_with_present_value() {
        unsafe { env::set_var("__PRESENT_VAR_COVERAGE_TEST__", "hello") };
        let mut missing: Vec<&str> = Vec::new();
        let val = require_env("__PRESENT_VAR_COVERAGE_TEST__", &mut missing);
        assert_eq!(val, "hello");
        assert!(missing.is_empty());
        unsafe { env::remove_var("__PRESENT_VAR_COVERAGE_TEST__") };
    }

    #[test]
    #[serial]
    fn test_build_content_api_urls_known_types() {
        unsafe {
            env::set_var("CONTENT_API_POST_URL", "http://post-api");
            env::set_var("CONTENT_API_BONUS_HUNTER_URL", "http://bonus-api");
            env::set_var("CONTENT_API_TOP_PICKS_URL", "http://top-picks-api");
        }
        let urls = build_content_api_urls();
        assert_eq!(urls.get("post"), Some(&"http://post-api".to_string()));
        assert_eq!(
            urls.get("bonus_hunter"),
            Some(&"http://bonus-api".to_string())
        );
        assert_eq!(
            urls.get("top_picks"),
            Some(&"http://top-picks-api".to_string())
        );
        unsafe {
            env::remove_var("CONTENT_API_POST_URL");
            env::remove_var("CONTENT_API_BONUS_HUNTER_URL");
            env::remove_var("CONTENT_API_TOP_PICKS_URL");
        }
    }

    #[test]
    #[serial]
    fn test_build_content_api_urls_unknown_extra_type() {
        unsafe {
            env::set_var("CONTENT_API_NEWS_ARTICLE_URL", "http://news-api");
        }
        let urls = build_content_api_urls();
        assert!(
            urls.contains_key("news_article"),
            "Expected news_article key from CONTENT_API_NEWS_ARTICLE_URL"
        );
        assert_eq!(
            urls.get("news_article"),
            Some(&"http://news-api".to_string())
        );
        unsafe {
            env::remove_var("CONTENT_API_NEWS_ARTICLE_URL");
        }
    }

    #[test]
    #[serial]
    fn test_build_content_api_urls_empty_value_not_inserted() {
        unsafe {
            env::set_var("CONTENT_API_POST_URL", "");
        }
        let urls = build_content_api_urls();
        // Empty value should not be inserted
        assert!(!urls.contains_key("post"));
        unsafe {
            env::remove_var("CONTENT_API_POST_URL");
        }
    }

    #[test]
    #[serial]
    fn test_from_env_success() {
        unsafe {
            env::set_var("DATABASE_URL", "postgres://x:x@localhost/x");
            env::set_var("READ_DATABASE_URL", "postgres://x:x@localhost/x");
            env::set_var("REDIS_URL", "redis://localhost");
            env::set_var("HTTP_PORT", "8080");
            env::set_var("PROFILE_API_URL", "http://localhost:8081");
            env::set_var("CONTENT_API_POST_URL", "http://localhost:8081");
            // Override optional settings too
            env::set_var("DB_MAX_CONNECTIONS", "10");
            env::set_var("DB_MIN_CONNECTIONS", "2");
            env::set_var("DB_ACQUIRE_TIMEOUT_SECS", "3");
            env::set_var("REDIS_POOL_SIZE", "5");
            env::set_var("RATE_LIMIT_WRITE_PER_MINUTE", "60");
            env::set_var("RATE_LIMIT_READ_PER_MINUTE", "2000");
            env::set_var("CACHE_TTL_LIKE_COUNTS_SECS", "600");
            env::set_var("CACHE_TTL_CONTENT_VALIDATION_SECS", "7200");
            env::set_var("CACHE_TTL_USER_STATUS_SECS", "120");
            env::set_var("CIRCUIT_BREAKER_FAILURE_THRESHOLD", "3");
            env::set_var("CIRCUIT_BREAKER_RECOVERY_TIMEOUT_SECS", "15");
            env::set_var("CIRCUIT_BREAKER_SUCCESS_THRESHOLD", "2");
            env::set_var("SHUTDOWN_TIMEOUT_SECS", "20");
            env::set_var("SSE_HEARTBEAT_INTERVAL_SECS", "10");
            env::set_var("SSE_BROADCAST_CAPACITY", "128");
            env::set_var("LEADERBOARD_REFRESH_INTERVAL_SECS", "30");
            env::set_var("LOG_LEVEL", "debug");
            env::set_var("GRPC_PORT", "50052");
            env::set_var("INTERNAL_TRANSPORT", "grpc");
        }

        let config = Config::from_env();
        assert_eq!(config.http_port, 8080);
        assert_eq!(config.grpc_port, 50052);
        assert_eq!(config.internal_transport, "grpc");
        assert!(config.use_grpc_transport());
        assert_eq!(config.database_url, "postgres://x:x@localhost/x");
        assert_eq!(config.read_database_url, "postgres://x:x@localhost/x");
        assert_eq!(config.redis_url, "redis://localhost");
        assert_eq!(config.profile_api_url, "http://localhost:8081");
        assert!(config.content_api_urls.contains_key("post"));
        assert_eq!(config.db_max_connections, 10);
        assert_eq!(config.db_min_connections, 2);
        assert_eq!(config.db_acquire_timeout_secs, 3);
        assert_eq!(config.redis_pool_size, 5);
        assert_eq!(config.rate_limit_write_per_minute, 60);
        assert_eq!(config.rate_limit_read_per_minute, 2000);
        assert_eq!(config.cache_ttl_like_counts_secs, 600);
        assert_eq!(config.cache_ttl_content_validation_secs, 7200);
        assert_eq!(config.circuit_breaker_failure_threshold, 3);
        assert_eq!(config.circuit_breaker_recovery_timeout_secs, 15);
        assert_eq!(config.circuit_breaker_success_threshold, 2);
        assert_eq!(config.server_concurrency_limit, 10000);
        assert_eq!(config.shutdown_timeout_secs, 20);
        assert_eq!(config.sse_heartbeat_interval_secs, 10);
        assert_eq!(config.sse_broadcast_capacity, 128);
        assert_eq!(config.leaderboard_refresh_interval_secs, 30);
        assert_eq!(config.log_level, "debug");

        unsafe {
            env::remove_var("DATABASE_URL");
            env::remove_var("READ_DATABASE_URL");
            env::remove_var("REDIS_URL");
            env::remove_var("HTTP_PORT");
            env::remove_var("PROFILE_API_URL");
            env::remove_var("CONTENT_API_POST_URL");
            env::remove_var("DB_MAX_CONNECTIONS");
            env::remove_var("DB_MIN_CONNECTIONS");
            env::remove_var("DB_ACQUIRE_TIMEOUT_SECS");
            env::remove_var("REDIS_POOL_SIZE");
            env::remove_var("RATE_LIMIT_WRITE_PER_MINUTE");
            env::remove_var("RATE_LIMIT_READ_PER_MINUTE");
            env::remove_var("CACHE_TTL_LIKE_COUNTS_SECS");
            env::remove_var("CACHE_TTL_CONTENT_VALIDATION_SECS");
            env::remove_var("CACHE_TTL_USER_STATUS_SECS");
            env::remove_var("CIRCUIT_BREAKER_FAILURE_THRESHOLD");
            env::remove_var("CIRCUIT_BREAKER_RECOVERY_TIMEOUT_SECS");
            env::remove_var("CIRCUIT_BREAKER_SUCCESS_THRESHOLD");
            env::remove_var("SHUTDOWN_TIMEOUT_SECS");
            env::remove_var("SSE_HEARTBEAT_INTERVAL_SECS");
            env::remove_var("SSE_BROADCAST_CAPACITY");
            env::remove_var("LEADERBOARD_REFRESH_INTERVAL_SECS");
            env::remove_var("LOG_LEVEL");
            env::remove_var("GRPC_PORT");
            env::remove_var("INTERNAL_TRANSPORT");
        }
    }

    #[test]
    #[serial]
    #[should_panic(expected = "Missing required environment variables")]
    fn test_from_env_panics_on_missing_required_vars() {
        // Ensure required vars are not set
        unsafe {
            env::remove_var("DATABASE_URL");
            env::remove_var("READ_DATABASE_URL");
            env::remove_var("REDIS_URL");
            env::remove_var("HTTP_PORT");
            env::remove_var("PROFILE_API_URL");
        }
        Config::from_env();
    }

    #[test]
    #[serial]
    #[should_panic(expected = "HTTP_PORT must be a valid port number")]
    fn test_from_env_panics_on_invalid_http_port() {
        unsafe {
            env::set_var("DATABASE_URL", "postgres://x:x@localhost/x");
            env::set_var("READ_DATABASE_URL", "postgres://x:x@localhost/x");
            env::set_var("REDIS_URL", "redis://localhost");
            env::set_var("HTTP_PORT", "not_a_port");
            env::set_var("PROFILE_API_URL", "http://localhost:8081");
            env::set_var("CONTENT_API_POST_URL", "http://localhost:8081");
        }
        let _config = Config::from_env();
        unsafe {
            env::remove_var("DATABASE_URL");
            env::remove_var("READ_DATABASE_URL");
            env::remove_var("REDIS_URL");
            env::remove_var("HTTP_PORT");
            env::remove_var("PROFILE_API_URL");
            env::remove_var("CONTENT_API_POST_URL");
        }
    }

    #[test]
    fn test_use_grpc_transport() {
        let mut config = Config::new_for_test();
        // Default is "http" -> false
        assert!(!config.use_grpc_transport());

        config.internal_transport = "grpc".to_string();
        assert!(config.use_grpc_transport());

        // Case-insensitive
        config.internal_transport = "GRPC".to_string();
        assert!(config.use_grpc_transport());

        config.internal_transport = "Grpc".to_string();
        assert!(config.use_grpc_transport());

        config.internal_transport = "http".to_string();
        assert!(!config.use_grpc_transport());

        config.internal_transport = "something_else".to_string();
        assert!(!config.use_grpc_transport());
    }

    #[test]
    #[serial]
    #[should_panic(expected = "No content API URLs configured")]
    fn test_from_env_panics_when_no_content_api_urls() {
        unsafe {
            env::set_var("DATABASE_URL", "postgres://x:x@localhost/x");
            env::set_var("READ_DATABASE_URL", "postgres://x:x@localhost/x");
            env::set_var("REDIS_URL", "redis://localhost");
            env::set_var("HTTP_PORT", "8080");
            env::set_var("PROFILE_API_URL", "http://localhost:8081");
            // Intentionally NOT setting any CONTENT_API_*_URL vars
            env::remove_var("CONTENT_API_POST_URL");
            env::remove_var("CONTENT_API_BONUS_HUNTER_URL");
            env::remove_var("CONTENT_API_TOP_PICKS_URL");
            env::remove_var("CONTENT_API_NEWS_ARTICLE_URL");
        }
        Config::from_env();
    }
}
