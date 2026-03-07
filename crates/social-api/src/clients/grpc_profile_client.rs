//! gRPC implementation of `TokenValidator`.
//!
//! Uses the `internal.v1.ProfileService/ValidateToken` RPC.
//! Caches results identically to the HTTP implementation.

use shared::errors::AppError;
use shared::types::AuthenticatedUser;
use uuid::Uuid;

use crate::cache::manager::CacheManager;
use crate::clients::profile_client::TokenValidator;
use crate::middleware::rate_limit::fnv1a_hash;
use crate::proto::internal_v1;
use crate::proto::internal_v1::profile_service_client::ProfileServiceClient;

/// gRPC implementation of `TokenValidator`.
pub struct GrpcTokenValidator {
    client: ProfileServiceClient<tonic::transport::Channel>,
    cache: CacheManager,
    cache_ttl_secs: u64,
}

impl GrpcTokenValidator {
    pub async fn new(
        endpoint: &str,
        cache: CacheManager,
        cache_ttl_secs: u64,
    ) -> Result<Self, AppError> {
        let client = ProfileServiceClient::connect(endpoint.to_string())
            .await
            .map_err(|e| AppError::DependencyUnavailable(format!("profile gRPC: {e}")))?;

        Ok(Self {
            client,
            cache,
            cache_ttl_secs,
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
        let response = client
            .validate_token(internal_v1::ValidateTokenRequest {
                token: token.to_string(),
            })
            .await;
        let latency = start.elapsed().as_secs_f64();

        match response {
            Ok(resp) => {
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
            Err(status) => {
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
        }
    }
}
