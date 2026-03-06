use shared::errors::AppError;
use shared::types::AuthenticatedUser;
use uuid::Uuid;

/// Trait for token validation — transport-swappable.
#[async_trait::async_trait]
pub trait TokenValidator: Send + Sync {
    /// Validate a bearer token and return the authenticated user.
    async fn validate(&self, token: &str) -> Result<AuthenticatedUser, AppError>;
}

/// HTTP implementation of TokenValidator.
pub struct HttpTokenValidator {
    http_client: reqwest::Client,
    profile_api_url: String,
    cache: crate::cache::manager::CacheManager,
    cache_ttl_secs: u64,
}

impl HttpTokenValidator {
    pub fn new(
        http_client: reqwest::Client,
        profile_api_url: String,
        cache: crate::cache::manager::CacheManager,
        cache_ttl_secs: u64,
    ) -> Self {
        Self {
            http_client,
            profile_api_url,
            cache,
            cache_ttl_secs,
        }
    }

    /// Compute a FNV-1a hash of the token and return it as a Redis cache key.
    /// The raw token is never stored — only its hash.
    fn token_cache_key(token: &str) -> String {
        const FNV_OFFSET: u64 = 0xcbf29ce484222325;
        const FNV_PRIME: u64 = 0x00000100000001B3;
        let mut hash = FNV_OFFSET;
        for byte in token.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        format!("tok:{hash:x}")
    }
}

#[async_trait::async_trait]
impl TokenValidator for HttpTokenValidator {
    async fn validate(&self, token: &str) -> Result<AuthenticatedUser, AppError> {
        let cache_key = Self::token_cache_key(token);
        if let Some(cached_json) = self.cache.get(&cache_key).await
            && let Ok(user) = serde_json::from_str::<AuthenticatedUser>(&cached_json)
        {
            return Ok(user);
        }
        // Corrupted cache entry — fall through to live validation

        let url = format!("{}/v1/auth/validate", self.profile_api_url);

        let start = std::time::Instant::now();
        let response = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
        let latency = start.elapsed().as_secs_f64();

        let response = match response {
            Ok(r) => {
                metrics::counter!(
                    "social_api_external_calls_total",
                    "service" => "profile_api",
                    "method" => "validate",
                    "status" => r.status().as_u16().to_string(),
                )
                .increment(1);
                metrics::histogram!(
                    "social_api_external_call_duration_seconds",
                    "service" => "profile_api",
                    "method" => "validate",
                )
                .record(latency);
                r
            }
            Err(e) => {
                metrics::counter!(
                    "social_api_external_calls_total",
                    "service" => "profile_api",
                    "method" => "validate",
                    "status" => "error",
                )
                .increment(1);
                metrics::histogram!(
                    "social_api_external_call_duration_seconds",
                    "service" => "profile_api",
                    "method" => "validate",
                )
                .record(latency);
                tracing::error!(
                    service = "profile_api",
                    error = %e,
                    "Profile API request failed"
                );
                return Err(AppError::DependencyUnavailable("profile_api".to_string()));
            }
        };

        if !response.status().is_success() {
            return Err(AppError::Unauthorized(
                "Invalid or expired token".to_string(),
            ));
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AppError::internal(format!("Failed to parse profile response: {e}")))?;

        let valid = body.get("valid").and_then(|v| v.as_bool()).unwrap_or(false);
        if !valid {
            return Err(AppError::Unauthorized(
                "Token validation returned invalid".to_string(),
            ));
        }

        // Parse user_id - strip "usr_" prefix from mock API format
        let user_id_str = body
            .get("user_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::internal("Missing user_id in profile response"))?;

        let uuid_str = user_id_str.strip_prefix("usr_").unwrap_or(user_id_str);
        let user_id = Uuid::parse_str(uuid_str)
            .map_err(|_| AppError::internal(format!("Invalid user_id format: {user_id_str}")))?;

        let display_name = body
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();

        let user = AuthenticatedUser {
            user_id,
            display_name,
        };

        if let Ok(json) = serde_json::to_string(&user) {
            self.cache.set(&cache_key, &json, self.cache_ttl_secs).await;
        }

        Ok(user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_cache_key_is_deterministic() {
        let k1 = HttpTokenValidator::token_cache_key("tok_user_1");
        let k2 = HttpTokenValidator::token_cache_key("tok_user_1");
        assert_eq!(k1, k2);
        assert!(k1.starts_with("tok:"));
    }

    #[test]
    fn test_token_cache_key_differs_per_token() {
        let k1 = HttpTokenValidator::token_cache_key("tok_user_1");
        let k2 = HttpTokenValidator::token_cache_key("tok_user_2");
        assert_ne!(k1, k2);
    }
}
