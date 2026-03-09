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
    cache: crate::cache::CacheManager,
    cache_ttl_secs: u64,
}

impl HttpTokenValidator {
    pub fn new(
        http_client: reqwest::Client,
        profile_api_url: String,
        cache: crate::cache::CacheManager,
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
        format!("tok:{:x}", crate::middleware::rate_limit::fnv1a_hash(token))
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
                super::metrics::record_external_call(
                    "profile_api",
                    "validate",
                    r.status().as_u16().to_string(),
                    latency,
                );
                r
            }
            Err(e) => {
                super::metrics::record_external_call(
                    "profile_api",
                    "validate",
                    "error".to_string(),
                    latency,
                );
                tracing::error!(
                    service = "profile_api",
                    error = %e,
                    "Profile API request failed"
                );
                return Err(AppError::DependencyUnavailable("profile_api".to_string()));
            }
        };

        if response.status().is_server_error() {
            tracing::error!(
                service = "profile_api",
                status = response.status().as_u16(),
                "Profile API returned server error"
            );
            return Err(AppError::DependencyUnavailable("profile_api".to_string()));
        }

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
    use std::net::SocketAddr;

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
    // 1. Cache hit — no HTTP server required
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_validate_cache_hit() {
        let (_scope, cache) = make_cache().await;

        let token = "tok_cache_hit_test";
        let cache_key = HttpTokenValidator::token_cache_key(token);

        // Populate the cache with a pre-built AuthenticatedUser
        let expected_user = AuthenticatedUser {
            user_id: uuid::Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
            display_name: "CacheUser".to_string(),
        };
        let json = serde_json::to_string(&expected_user).unwrap();
        cache.set(&cache_key, &json, 300).await;

        // Build the validator pointing at a non-existent URL — if an HTTP call
        // were made it would fail, proving the cache is used exclusively.
        let validator = HttpTokenValidator::new(
            reqwest::Client::new(),
            "http://127.0.0.1:19997".to_string(), // unreachable — no server
            cache,
            300,
        );

        let result = validator.validate(token).await;
        let user = result.expect("should return Ok from cache");
        assert_eq!(user.user_id, expected_user.user_id);
        assert_eq!(user.display_name, expected_user.display_name);
    }

    // -------------------------------------------------------------------------
    // 2. Unauthorized — mock server returns 401
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_validate_unauthorized() {
        let (addr, _handle) = spawn_mock_server(401, "").await;
        let (_scope, cache) = make_cache().await;

        let validator =
            HttpTokenValidator::new(reqwest::Client::new(), format!("http://{addr}"), cache, 300);

        let result = validator.validate("tok_bad_token").await;
        let err = result.expect_err("should fail with Unauthorized");
        assert!(
            matches!(err, AppError::Unauthorized(_)),
            "expected Unauthorized, got: {err:?}"
        );
    }

    // -------------------------------------------------------------------------
    // 3. DependencyUnavailable — mock server returns 500
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_validate_dependency_unavailable() {
        let (addr, _handle) = spawn_mock_server(500, "").await;
        let (_scope, cache) = make_cache().await;

        let validator =
            HttpTokenValidator::new(reqwest::Client::new(), format!("http://{addr}"), cache, 300);

        let result = validator.validate("tok_server_error").await;
        let err = result.expect_err("should fail with DependencyUnavailable");
        assert!(
            matches!(err, AppError::DependencyUnavailable(_)),
            "expected DependencyUnavailable, got: {err:?}"
        );
    }

    // -------------------------------------------------------------------------
    // 4. Success + caching — mock server returns 200 with valid JSON
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_validate_success_and_caches() {
        let user_id = uuid::Uuid::parse_str("aaaabbbb-cccc-dddd-eeee-ffffffffffff").unwrap();
        let body = r#"{"user_id":"aaaabbbb-cccc-dddd-eeee-ffffffffffff","valid":true,"display_name":"TestUser"}"#;

        let (addr, abort_handle) = spawn_mock_server(200, body).await;
        let (_scope, cache) = make_cache().await;

        let validator =
            HttpTokenValidator::new(reqwest::Client::new(), format!("http://{addr}"), cache, 300);

        let token = "tok_success_test";

        // First call: goes to the mock server, returns the user, caches it
        let result = validator.validate(token).await;
        let user = result.expect("first validate should succeed");
        assert_eq!(user.user_id, user_id);
        assert_eq!(user.display_name, "TestUser");

        // Stop the mock server — any further HTTP call would fail
        abort_handle.abort();

        // Second call: must be served from cache (no live server)
        let result2 = validator.validate(token).await;
        let user2 = result2.expect("second validate should succeed from cache");
        assert_eq!(user2.user_id, user_id);
        assert_eq!(user2.display_name, "TestUser");
    }
}
