use shared::errors::AppError;
use uuid::Uuid;

use crate::cache::manager::CacheManager;
use crate::config::Config;

/// Trait for content validation — transport-swappable (HTTP or gRPC via INTERNAL_TRANSPORT).
#[async_trait::async_trait]
pub trait ContentValidator: Send + Sync {
    /// Check if content exists. Returns Ok(true) if valid, Ok(false) if not found.
    async fn validate(&self, content_type: &str, content_id: Uuid) -> Result<bool, AppError>;
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
    async fn validate(&self, content_type: &str, content_id: Uuid) -> Result<bool, AppError> {
        // Check config for known content type
        let base_url = self
            .config
            .content_api_url(content_type)
            .ok_or_else(|| AppError::ContentTypeUnknown(content_type.to_string()))?;

        // Check cache first
        let cache_key = Self::cache_key(content_type, content_id);
        if let Some(cached) = self.cache.get(&cache_key).await {
            return Ok(cached == "1");
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
            Ok(true)
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
            Ok(false)
        }
    }
}
