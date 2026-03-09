use shared::errors::AppError;
use uuid::Uuid;

use crate::cache::CacheManager;
use crate::config::Config;

/// Outcome of a content validation check.
///
/// Distinguishes between results served from cache (no real remote call made)
/// and results from an actual remote call (HTTP or gRPC). Callers use this to
/// signal the circuit breaker only when a real call was made, preventing cached
/// hits from masking a failing upstream service.
#[derive(Debug, Clone, Copy)]
pub enum ValidationOutcome {
    /// Result served from cache — circuit breaker must NOT be signaled.
    Cached(bool),
    /// Result from a real remote call — circuit breaker MUST be signaled.
    Remote(bool),
}

impl ValidationOutcome {
    /// Extract the boolean validity from either variant.
    #[allow(dead_code)]
    pub fn is_valid(self) -> bool {
        match self {
            Self::Cached(v) | Self::Remote(v) => v,
        }
    }
}

/// Trait for content validation — transport-swappable (HTTP or gRPC via INTERNAL_TRANSPORT).
#[async_trait::async_trait]
pub trait ContentValidator: Send + Sync {
    /// Check if content exists.
    ///
    /// Returns `Ok(Cached(..))` when served from cache (no remote call made) or
    /// `Ok(Remote(..))` when a real HTTP/gRPC call was made. Returns `Err` only
    /// when the remote call itself failed (network error, 5xx, timeout).
    async fn validate(
        &self,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<ValidationOutcome, AppError>;
}

/// HTTP implementation of ContentValidator.
pub struct HttpContentValidator {
    http_client: reqwest::Client,
    cache: CacheManager,
    config: Config,
}

impl HttpContentValidator {
    pub fn new(http_client: reqwest::Client, cache: CacheManager, config: Config) -> Self {
        Self {
            http_client,
            cache,
            config,
        }
    }

    /// Cache key for content validation.
    fn cache_key(content_type: &str, content_id: Uuid) -> String {
        format!("cv:{content_type}:{content_id}")
    }
}

#[async_trait::async_trait]
impl ContentValidator for HttpContentValidator {
    async fn validate(
        &self,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<ValidationOutcome, AppError> {
        // Check config for known content type
        let base_url = self
            .config
            .content_api_url(content_type)
            .ok_or_else(|| AppError::ContentTypeUnknown(content_type.to_string()))?;

        // Check cache first — return Cached so caller skips circuit breaker signaling
        let cache_key = Self::cache_key(content_type, content_id);
        if let Some(cached) = self.cache.get(&cache_key).await {
            return Ok(ValidationOutcome::Cached(cached == "1"));
        }

        // Call Content API with metrics instrumentation
        let url = format!("{base_url}/v1/{content_type}/{content_id}");
        let start = std::time::Instant::now();
        let response = self
            .http_client
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
        let latency = start.elapsed().as_secs_f64();

        let response = match response {
            Ok(r) => {
                super::metrics::record_external_call(
                    "content_api",
                    "validate",
                    r.status().as_u16().to_string(),
                    latency,
                );
                r
            }
            Err(e) => {
                super::metrics::record_external_call(
                    "content_api",
                    "validate",
                    "error".to_string(),
                    latency,
                );
                tracing::error!(
                    service = "content_api",
                    content_type = content_type,
                    error = %e,
                    "Content API request failed"
                );
                return Err(AppError::DependencyUnavailable("content_api".to_string()));
            }
        };

        let status = response.status();

        if status.is_success() {
            // Content exists — cache as valid
            self.cache
                .set(
                    &cache_key,
                    "1",
                    self.config.cache_ttl_content_validation_secs,
                )
                .await;
            tracing::debug!(
                service = "content_api",
                content_type = content_type,
                content_id = %content_id,
                valid = true,
                "Content validation result"
            );
            Ok(ValidationOutcome::Remote(true))
        } else if status.is_server_error() {
            // 5xx — upstream failure, propagate as dependency error (do NOT cache)
            tracing::error!(
                service = "content_api",
                content_type = content_type,
                content_id = %content_id,
                status = status.as_u16(),
                "Content API returned server error"
            );
            Err(AppError::DependencyUnavailable("content_api".to_string()))
        } else {
            // 4xx (including 404) — content not found, cache briefly to avoid hammering
            self.cache.set(&cache_key, "0", 60).await;
            tracing::debug!(
                service = "content_api",
                content_type = content_type,
                content_id = %content_id,
                valid = false,
                "Content validation result"
            );
            Ok(ValidationOutcome::Remote(false))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use uuid::Uuid;

    /// Spawn an inline mock Axum server that always returns the given status + body.
    /// Returns the bound address and an abort handle to stop it.
    async fn spawn_mock_server(
        status: u16,
        body: &'static str,
    ) -> (SocketAddr, tokio::task::AbortHandle) {
        use axum::{Router, http::StatusCode, routing::get};
        use tokio::net::TcpListener;

        let app = Router::new().route(
            "/{*path}",
            get(move || async move { (StatusCode::from_u16(status).unwrap(), body) }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        })
        .abort_handle();
        (addr, handle)
    }

    /// Build a CacheManager backed by real Redis (via isolated_scope).
    async fn make_cache() -> (
        crate::test_containers::TestScope,
        crate::cache::CacheManager,
    ) {
        let scope = crate::test_containers::isolated_scope().await;
        let mut config = crate::config::Config::new_for_test();
        config.redis_url = scope.redis_url.clone();
        let pool = crate::cache::create_pool(&config).await.unwrap();
        let cache = crate::cache::CacheManager::new(pool, &config);
        (scope, cache)
    }

    // -------------------------------------------------------------------------
    // 1. Unknown content type returns ContentTypeUnknown error (no HTTP/Redis)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_unknown_content_type_returns_error() {
        let (_scope, cache) = make_cache().await;
        let http_client = reqwest::Client::new();
        let config = crate::config::Config::new_for_test(); // no "unknown_type" URL in config
        let validator = HttpContentValidator::new(http_client, cache, config);
        let content_id = Uuid::new_v4();

        let result = validator.validate("unknown_type", content_id).await;
        let err = result.expect_err("should fail for unknown content type");
        assert!(
            matches!(err, AppError::ContentTypeUnknown(_)),
            "expected ContentTypeUnknown, got: {err:?}"
        );
    }

    // -------------------------------------------------------------------------
    // 2. Cache hit returns Cached(true) without making an HTTP call
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_cache_hit_returns_cached_valid() {
        let (_scope, cache) = make_cache().await;

        let content_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
        let cache_key = format!("cv:article:{content_id}");

        // Pre-populate the cache with "1" (valid)
        cache.set(&cache_key, "1", 300).await;

        // Insert "article" into the config pointing at an unreachable URL — any
        // real HTTP request would fail, proving the cache is used.
        let mut config = crate::config::Config::new_for_test();
        config
            .content_api_urls
            .insert("article".to_string(), "http://127.0.0.1:19996".to_string());

        let validator = HttpContentValidator::new(reqwest::Client::new(), cache, config);

        let result = validator.validate("article", content_id).await;
        let outcome = result.expect("should return Ok from cache");
        assert!(
            matches!(outcome, ValidationOutcome::Cached(true)),
            "expected Cached(true), got: {outcome:?}"
        );
    }

    // -------------------------------------------------------------------------
    // 3. Remote call succeeds (200) → Remote(true) + cache populated with "1"
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_validate_remote_success() {
        let (addr, _handle) = spawn_mock_server(200, "{}").await;
        let (_scope, cache) = make_cache().await;

        let content_id = Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap();
        let cache_key = format!("cv:article:{content_id}");

        let mut config = crate::config::Config::new_for_test();
        config
            .content_api_urls
            .insert("article".to_string(), format!("http://{addr}"));

        let validator = HttpContentValidator::new(reqwest::Client::new(), cache.clone(), config);

        let result = validator.validate("article", content_id).await;
        let outcome = result.expect("remote 200 should succeed");
        assert!(
            matches!(outcome, ValidationOutcome::Remote(true)),
            "expected Remote(true), got: {outcome:?}"
        );

        // Verify the cache was populated with "1"
        let cached = cache.get(&cache_key).await;
        assert_eq!(
            cached,
            Some("1".to_string()),
            "cache should contain '1' after successful validation"
        );
    }

    // -------------------------------------------------------------------------
    // 4. Remote call returns 404 → Remote(false) + cache populated with "0"
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_validate_not_found() {
        let (addr, _handle) = spawn_mock_server(404, "").await;
        let (_scope, cache) = make_cache().await;

        let content_id = Uuid::parse_str("44444444-4444-4444-4444-444444444444").unwrap();
        let cache_key = format!("cv:article:{content_id}");

        let mut config = crate::config::Config::new_for_test();
        config
            .content_api_urls
            .insert("article".to_string(), format!("http://{addr}"));

        let validator = HttpContentValidator::new(reqwest::Client::new(), cache.clone(), config);

        let result = validator.validate("article", content_id).await;
        let outcome = result.expect("remote 404 should return Ok(Remote(false))");
        assert!(
            matches!(outcome, ValidationOutcome::Remote(false)),
            "expected Remote(false), got: {outcome:?}"
        );

        // Verify the cache was populated with "0"
        let cached = cache.get(&cache_key).await;
        assert_eq!(
            cached,
            Some("0".to_string()),
            "cache should contain '0' after not-found response"
        );
    }

    // -------------------------------------------------------------------------
    // 5. Remote call returns 500 → DependencyUnavailable error, nothing cached
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_validate_server_error() {
        let (addr, _handle) = spawn_mock_server(500, "").await;
        let (_scope, cache) = make_cache().await;

        let content_id = Uuid::parse_str("55555555-5555-5555-5555-555555555555").unwrap();
        let cache_key = format!("cv:article:{content_id}");

        let mut config = crate::config::Config::new_for_test();
        config
            .content_api_urls
            .insert("article".to_string(), format!("http://{addr}"));

        let validator = HttpContentValidator::new(reqwest::Client::new(), cache.clone(), config);

        let result = validator.validate("article", content_id).await;
        let err = result.expect_err("remote 500 should return Err");
        assert!(
            matches!(err, AppError::DependencyUnavailable(_)),
            "expected DependencyUnavailable, got: {err:?}"
        );

        // Verify nothing was cached for the 5xx response
        let cached = cache.get(&cache_key).await;
        assert_eq!(cached, None, "cache should remain empty after server error");
    }
}
