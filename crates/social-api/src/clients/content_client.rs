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
