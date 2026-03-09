//! gRPC implementation of `ContentValidator`.
//!
//! Uses the `internal.v1.ContentService/Validate` RPC to check content existence.
//! Caches results identically to the HTTP implementation.

use shared::errors::AppError;
use std::time::Duration;
use uuid::Uuid;

use crate::cache::CacheManager;
use crate::config::Config;
use crate::proto::internal_v1;
use crate::proto::internal_v1::content_service_client::ContentServiceClient;

/// gRPC implementation of `ContentValidator`.
pub struct GrpcContentValidator {
    client: ContentServiceClient<tonic::transport::Channel>,
    cache: CacheManager,
    config: Config,
    request_timeout: Duration,
}

impl GrpcContentValidator {
    pub async fn new(
        endpoint: &str,
        cache: CacheManager,
        config: Config,
        request_timeout: Duration,
    ) -> Result<Self, AppError> {
        let client = ContentServiceClient::connect(endpoint.to_string())
            .await
            .map_err(|e| AppError::DependencyUnavailable(format!("content gRPC: {e}")))?;

        Ok(Self {
            client,
            cache,
            config,
            request_timeout,
        })
    }

    fn cache_key(content_type: &str, content_id: Uuid) -> String {
        format!("cv:{content_type}:{content_id}")
    }
}

#[async_trait::async_trait]
impl super::content_client::ContentValidator for GrpcContentValidator {
    async fn validate(
        &self,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<super::content_client::ValidationOutcome, AppError> {
        use super::content_client::ValidationOutcome;

        // Check cache first — return Cached so caller skips circuit breaker signaling
        let cache_key = Self::cache_key(content_type, content_id);
        if let Some(cached) = self.cache.get(&cache_key).await {
            return Ok(ValidationOutcome::Cached(cached == "1"));
        }

        let start = std::time::Instant::now();
        let mut client = self.client.clone();
        let response = tokio::time::timeout(
            self.request_timeout,
            client.validate(internal_v1::ValidateContentRequest {
                content_type: content_type.to_string(),
                content_id: content_id.to_string(),
            }),
        )
        .await;
        let latency = start.elapsed().as_secs_f64();

        match response {
            Ok(Ok(resp)) => {
                let valid = resp.into_inner().exists;
                super::metrics::record_external_call(
                    "content_api",
                    "validate_grpc",
                    "OK".to_string(),
                    latency,
                );

                let ttl = if valid {
                    self.config.cache_ttl_content_validation_secs
                } else {
                    60
                };
                self.cache
                    .set(&cache_key, if valid { "1" } else { "0" }, ttl)
                    .await;

                Ok(ValidationOutcome::Remote(valid))
            }
            Ok(Err(status)) => {
                super::metrics::record_external_call(
                    "content_api",
                    "validate_grpc",
                    status.code().to_string(),
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
            Err(_timeout) => {
                super::metrics::record_external_call(
                    "content_api",
                    "validate_grpc",
                    "timeout".to_string(),
                    latency,
                );
                Err(AppError::DependencyUnavailable("content_api".to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::create_pool;
    use crate::clients::content_client::ContentValidator;
    use crate::proto::internal_v1::{
        ValidateContentRequest, ValidateContentResponse,
        content_service_server::{ContentService, ContentServiceServer},
    };
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::{Request, Response, Status};

    // ── Mock service implementations ──────────────────────────────────────

    #[derive(Clone)]
    struct FixedContent {
        exists: bool,
    }

    #[tonic::async_trait]
    impl ContentService for FixedContent {
        async fn validate(
            &self,
            _: Request<ValidateContentRequest>,
        ) -> Result<Response<ValidateContentResponse>, Status> {
            Ok(Response::new(ValidateContentResponse {
                exists: self.exists,
            }))
        }
    }

    #[derive(Clone)]
    struct ErrContent {
        code: tonic::Code,
    }

    #[tonic::async_trait]
    impl ContentService for ErrContent {
        async fn validate(
            &self,
            _: Request<ValidateContentRequest>,
        ) -> Result<Response<ValidateContentResponse>, Status> {
            Err(Status::new(self.code, "mock error"))
        }
    }

    #[derive(Clone)]
    struct SlowContent;

    #[tonic::async_trait]
    impl ContentService for SlowContent {
        async fn validate(
            &self,
            _: Request<ValidateContentRequest>,
        ) -> Result<Response<ValidateContentResponse>, Status> {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(Response::new(ValidateContentResponse { exists: true }))
        }
    }

    async fn spawn_content<S: ContentService>(svc: S) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(ContentServiceServer::new(svc))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .ok();
        });
        tokio::time::sleep(Duration::from_millis(15)).await;
        format!("http://127.0.0.1:{port}")
    }

    struct Harness {
        _scope: crate::test_containers::TestScope,
        cache: CacheManager,
        config: Config,
    }

    async fn make_harness() -> Harness {
        let scope = crate::test_containers::isolated_scope().await;
        let mut config = Config::new_for_test();
        config.redis_url = scope.redis_url.clone();
        let pool = create_pool(&config).await.unwrap();
        let cache = CacheManager::new(pool, &config);
        Harness {
            _scope: scope,
            cache,
            config,
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────

    #[test]
    fn cache_key_has_cv_prefix() {
        let id = Uuid::new_v4();
        let key = GrpcContentValidator::cache_key("article", id);
        assert_eq!(key, format!("cv:article:{id}"));
    }

    #[tokio::test]
    async fn validate_remote_true_when_content_exists() {
        let h = make_harness().await;
        let endpoint = spawn_content(FixedContent { exists: true }).await;
        let v = GrpcContentValidator::new(&endpoint, h.cache, h.config, Duration::from_secs(5))
            .await
            .unwrap();
        let result = v.validate("post", Uuid::new_v4()).await.unwrap();
        use crate::clients::content_client::ValidationOutcome;
        assert!(matches!(result, ValidationOutcome::Remote(true)));
    }

    #[tokio::test]
    async fn validate_remote_false_when_content_missing() {
        let h = make_harness().await;
        let endpoint = spawn_content(FixedContent { exists: false }).await;
        let v = GrpcContentValidator::new(&endpoint, h.cache, h.config, Duration::from_secs(5))
            .await
            .unwrap();
        let result = v.validate("post", Uuid::new_v4()).await.unwrap();
        use crate::clients::content_client::ValidationOutcome;
        assert!(matches!(result, ValidationOutcome::Remote(false)));
    }

    #[tokio::test]
    async fn validate_returns_cached_result_on_cache_hit() {
        let h = make_harness().await;
        let id = Uuid::new_v4();
        let key = GrpcContentValidator::cache_key("post", id);
        h.cache.set(&key, "1", 300).await;

        // Server would fail — proves cache is consulted first
        let endpoint = spawn_content(ErrContent {
            code: tonic::Code::Internal,
        })
        .await;
        let v = GrpcContentValidator::new(&endpoint, h.cache, h.config, Duration::from_secs(5))
            .await
            .unwrap();
        let result = v.validate("post", id).await.unwrap();
        use crate::clients::content_client::ValidationOutcome;
        assert!(matches!(result, ValidationOutcome::Cached(true)));
    }

    #[tokio::test]
    async fn validate_returns_error_on_grpc_failure() {
        let h = make_harness().await;
        let endpoint = spawn_content(ErrContent {
            code: tonic::Code::Internal,
        })
        .await;
        let v = GrpcContentValidator::new(&endpoint, h.cache, h.config, Duration::from_secs(5))
            .await
            .unwrap();
        let err = v.validate("post", Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(err, AppError::DependencyUnavailable(_)));
    }

    #[tokio::test]
    async fn validate_returns_error_on_timeout() {
        let h = make_harness().await;
        let endpoint = spawn_content(SlowContent).await;
        let v = GrpcContentValidator::new(&endpoint, h.cache, h.config, Duration::from_millis(50))
            .await
            .unwrap();
        let err = v.validate("post", Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(err, AppError::DependencyUnavailable(_)));
    }
}
