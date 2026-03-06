//! gRPC implementation of `ContentValidator`.
//!
//! Uses the `internal.v1.ContentService/Validate` RPC to check content existence.
//! Caches results identically to the HTTP implementation.

use shared::errors::AppError;
use uuid::Uuid;

use crate::cache::manager::CacheManager;
use crate::config::Config;
use crate::proto::internal_v1;
use crate::proto::internal_v1::content_service_client::ContentServiceClient;

/// gRPC implementation of `ContentValidator`.
pub struct GrpcContentValidator {
    client: ContentServiceClient<tonic::transport::Channel>,
    cache: CacheManager,
    config: Config,
}

impl GrpcContentValidator {
    pub async fn new(
        endpoint: &str,
        cache: CacheManager,
        config: Config,
    ) -> Result<Self, AppError> {
        let client = ContentServiceClient::connect(endpoint.to_string())
            .await
            .map_err(|e| AppError::DependencyUnavailable(format!("content gRPC: {e}")))?;

        Ok(Self {
            client,
            cache,
            config,
        })
    }

    fn cache_key(content_type: &str, content_id: Uuid) -> String {
        format!("cv:{content_type}:{content_id}")
    }
}

#[async_trait::async_trait]
impl super::content_client::ContentValidator for GrpcContentValidator {
    async fn validate(&self, content_type: &str, content_id: Uuid) -> Result<bool, AppError> {
        // Check cache first (same key format as HTTP client)
        let cache_key = Self::cache_key(content_type, content_id);
        if let Some(cached) = self.cache.get(&cache_key).await {
            return Ok(cached == "1");
        }

        let start = std::time::Instant::now();
        let mut client = self.client.clone();
        let response = client
            .validate(internal_v1::ValidateContentRequest {
                content_type: content_type.to_string(),
                content_id: content_id.to_string(),
            })
            .await;
        let latency = start.elapsed().as_secs_f64();

        match response {
            Ok(resp) => {
                let valid = resp.into_inner().exists;
                super::metrics::record_external_call("content_api", "validate_grpc", "OK", latency);

                let ttl = if valid {
                    self.config.cache_ttl_content_validation_secs
                } else {
                    60
                };
                self.cache
                    .set(&cache_key, if valid { "1" } else { "0" }, ttl)
                    .await;

                Ok(valid)
            }
            Err(status) => {
                super::metrics::record_external_call(
                    "content_api",
                    "validate_grpc",
                    &status.code().to_string(),
                    latency,
                );
                tracing::error!(
                    service = "content_api",
                    transport = "grpc",
                    error = %status,
                    "Content API gRPC request failed"
                );
                Err(AppError::DependencyUnavailable("content_api".to_string()))
            }
        }
    }
}
