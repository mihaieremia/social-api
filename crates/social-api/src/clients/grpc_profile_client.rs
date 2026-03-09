//! gRPC implementation of `TokenValidator`.
//!
//! Uses the `internal.v1.ProfileService/ValidateToken` RPC.
//! Caches results identically to the HTTP implementation.

use shared::errors::AppError;
use shared::types::AuthenticatedUser;
use std::time::Duration;
use uuid::Uuid;

use crate::cache::CacheManager;
use crate::clients::profile_client::TokenValidator;
use crate::middleware::rate_limit::fnv1a_hash;
use crate::proto::internal_v1;
use crate::proto::internal_v1::profile_service_client::ProfileServiceClient;

/// gRPC implementation of `TokenValidator`.
pub struct GrpcTokenValidator {
    client: ProfileServiceClient<tonic::transport::Channel>,
    cache: CacheManager,
    cache_ttl_secs: u64,
    request_timeout: Duration,
}

impl GrpcTokenValidator {
    pub async fn new(
        endpoint: &str,
        cache: CacheManager,
        cache_ttl_secs: u64,
        request_timeout: Duration,
    ) -> Result<Self, AppError> {
        let client = ProfileServiceClient::connect(endpoint.to_string())
            .await
            .map_err(|e| AppError::DependencyUnavailable(format!("profile gRPC: {e}")))?;

        Ok(Self {
            client,
            cache,
            cache_ttl_secs,
            request_timeout,
        })
    }

    fn token_cache_key(token: &str) -> String {
        format!("tok:{:x}", fnv1a_hash(token))
    }
}

#[async_trait::async_trait]
impl TokenValidator for GrpcTokenValidator {
    async fn validate(&self, token: &str) -> Result<AuthenticatedUser, AppError> {
        // Check cache first
        let cache_key = Self::token_cache_key(token);
        if let Some(cached_json) = self.cache.get(&cache_key).await
            && let Ok(user) = serde_json::from_str::<AuthenticatedUser>(&cached_json)
        {
            return Ok(user);
        }

        let start = std::time::Instant::now();
        let mut client = self.client.clone();
        let response = tokio::time::timeout(
            self.request_timeout,
            client.validate_token(internal_v1::ValidateTokenRequest {
                token: token.to_string(),
            }),
        )
        .await;
        let latency = start.elapsed().as_secs_f64();

        match response {
            Ok(Ok(resp)) => {
                let inner = resp.into_inner();
                super::metrics::record_external_call(
                    "profile_api",
                    "validate_grpc",
                    "OK".to_string(),
                    latency,
                );

                if !inner.valid {
                    return Err(AppError::Unauthorized(
                        "Token validation returned invalid".to_string(),
                    ));
                }

                let user_id = Uuid::parse_str(&inner.user_id).map_err(|_| {
                    AppError::internal(format!("Invalid user_id from gRPC: {}", inner.user_id))
                })?;

                let user = AuthenticatedUser {
                    user_id,
                    display_name: inner.display_name,
                };

                if let Ok(json) = serde_json::to_string(&user) {
                    self.cache.set(&cache_key, &json, self.cache_ttl_secs).await;
                }

                Ok(user)
            }
            Ok(Err(status)) => {
                super::metrics::record_external_call(
                    "profile_api",
                    "validate_grpc",
                    status.code().to_string(),
                    latency,
                );

                if status.code() == tonic::Code::Unauthenticated {
                    return Err(AppError::Unauthorized(
                        "Invalid or expired token".to_string(),
                    ));
                }

                Err(AppError::DependencyUnavailable("profile_api".to_string()))
            }
            Err(_) => {
                super::metrics::record_external_call(
                    "profile_api",
                    "validate_grpc",
                    "timeout".to_string(),
                    latency,
                );
                Err(AppError::DependencyUnavailable("profile_api".to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::create_pool;
    use crate::clients::profile_client::TokenValidator;
    use crate::config::Config;
    use crate::proto::internal_v1::{
        ValidateTokenRequest, ValidateTokenResponse,
        profile_service_server::{ProfileService, ProfileServiceServer},
    };
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::{Request, Response, Status};

    // ── Mock service implementations ──────────────────────────────────────

    #[derive(Clone)]
    struct ValidProfile {
        user_id: String,
        display_name: String,
    }

    #[tonic::async_trait]
    impl ProfileService for ValidProfile {
        async fn validate_token(
            &self,
            _: Request<ValidateTokenRequest>,
        ) -> Result<Response<ValidateTokenResponse>, Status> {
            Ok(Response::new(ValidateTokenResponse {
                valid: true,
                user_id: self.user_id.clone(),
                display_name: self.display_name.clone(),
            }))
        }
    }

    #[derive(Clone)]
    struct InvalidProfile;

    #[tonic::async_trait]
    impl ProfileService for InvalidProfile {
        async fn validate_token(
            &self,
            _: Request<ValidateTokenRequest>,
        ) -> Result<Response<ValidateTokenResponse>, Status> {
            Ok(Response::new(ValidateTokenResponse {
                valid: false,
                user_id: String::new(),
                display_name: String::new(),
            }))
        }
    }

    #[derive(Clone)]
    struct ErrProfile {
        code: tonic::Code,
    }

    #[tonic::async_trait]
    impl ProfileService for ErrProfile {
        async fn validate_token(
            &self,
            _: Request<ValidateTokenRequest>,
        ) -> Result<Response<ValidateTokenResponse>, Status> {
            Err(Status::new(self.code, "mock error"))
        }
    }

    #[derive(Clone)]
    struct SlowProfile;

    #[tonic::async_trait]
    impl ProfileService for SlowProfile {
        async fn validate_token(
            &self,
            _: Request<ValidateTokenRequest>,
        ) -> Result<Response<ValidateTokenResponse>, Status> {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Err(Status::cancelled("too slow"))
        }
    }

    #[derive(Clone)]
    struct BadUuidProfile;

    #[tonic::async_trait]
    impl ProfileService for BadUuidProfile {
        async fn validate_token(
            &self,
            _: Request<ValidateTokenRequest>,
        ) -> Result<Response<ValidateTokenResponse>, Status> {
            Ok(Response::new(ValidateTokenResponse {
                valid: true,
                user_id: "not-a-uuid".to_string(),
                display_name: "User".to_string(),
            }))
        }
    }

    async fn spawn_profile<S: ProfileService>(svc: S) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(ProfileServiceServer::new(svc))
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
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────

    #[test]
    fn token_cache_key_has_tok_prefix() {
        let key = GrpcTokenValidator::token_cache_key("Bearer abc123");
        assert!(key.starts_with("tok:"));
    }

    #[tokio::test]
    async fn validate_returns_user_for_valid_token() {
        let h = make_harness().await;
        let user_id = Uuid::new_v4();
        let endpoint = spawn_profile(ValidProfile {
            user_id: user_id.to_string(),
            display_name: "Alice".to_string(),
        })
        .await;
        let v = GrpcTokenValidator::new(&endpoint, h.cache, 300, Duration::from_secs(5))
            .await
            .unwrap();
        let user = v.validate("tok_valid").await.unwrap();
        assert_eq!(user.user_id, user_id);
        assert_eq!(user.display_name, "Alice");
    }

    #[tokio::test]
    async fn validate_returns_cached_user_on_cache_hit() {
        let h = make_harness().await;
        let user_id = Uuid::new_v4();
        let key = GrpcTokenValidator::token_cache_key("tok_cached");
        let user = AuthenticatedUser {
            user_id,
            display_name: "Cached".to_string(),
        };
        h.cache
            .set(&key, &serde_json::to_string(&user).unwrap(), 300)
            .await;

        // Server would fail — proves cache is consulted first
        let endpoint = spawn_profile(ErrProfile {
            code: tonic::Code::Internal,
        })
        .await;
        let v = GrpcTokenValidator::new(&endpoint, h.cache, 300, Duration::from_secs(5))
            .await
            .unwrap();
        let result = v.validate("tok_cached").await.unwrap();
        assert_eq!(result.user_id, user_id);
    }

    #[tokio::test]
    async fn validate_returns_unauthorized_when_valid_false() {
        let h = make_harness().await;
        let endpoint = spawn_profile(InvalidProfile).await;
        let v = GrpcTokenValidator::new(&endpoint, h.cache, 300, Duration::from_secs(5))
            .await
            .unwrap();
        let err = v.validate("tok_invalid").await.unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(_)));
    }

    #[tokio::test]
    async fn validate_returns_unauthorized_for_unauthenticated_status() {
        let h = make_harness().await;
        let endpoint = spawn_profile(ErrProfile {
            code: tonic::Code::Unauthenticated,
        })
        .await;
        let v = GrpcTokenValidator::new(&endpoint, h.cache, 300, Duration::from_secs(5))
            .await
            .unwrap();
        let err = v.validate("tok_unauth").await.unwrap_err();
        assert!(matches!(err, AppError::Unauthorized(_)));
    }

    #[tokio::test]
    async fn validate_returns_dependency_unavailable_for_other_grpc_error() {
        let h = make_harness().await;
        let endpoint = spawn_profile(ErrProfile {
            code: tonic::Code::Internal,
        })
        .await;
        let v = GrpcTokenValidator::new(&endpoint, h.cache, 300, Duration::from_secs(5))
            .await
            .unwrap();
        let err = v.validate("tok_grpc_err").await.unwrap_err();
        assert!(matches!(err, AppError::DependencyUnavailable(_)));
    }

    #[tokio::test]
    async fn validate_returns_dependency_unavailable_on_timeout() {
        let h = make_harness().await;
        let endpoint = spawn_profile(SlowProfile).await;
        let v = GrpcTokenValidator::new(&endpoint, h.cache, 300, Duration::from_millis(50))
            .await
            .unwrap();
        let err = v.validate("tok_timeout").await.unwrap_err();
        assert!(matches!(err, AppError::DependencyUnavailable(_)));
    }

    #[tokio::test]
    async fn validate_returns_error_for_invalid_uuid_in_response() {
        let h = make_harness().await;
        let endpoint = spawn_profile(BadUuidProfile).await;
        let v = GrpcTokenValidator::new(&endpoint, h.cache, 300, Duration::from_secs(5))
            .await
            .unwrap();
        let result = v.validate("tok_bad_uuid").await;
        assert!(result.is_err());
    }
}
