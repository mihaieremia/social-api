use shared::errors::AppError;
use shared::types::AuthenticatedUser;

use crate::clients::circuit_breaker::CircuitBreaker;
use crate::clients::profile_client::TokenValidator;

/// Reasons authentication can fail, mapped to `AppError` by extractors/interceptors.
///
/// - `MissingToken` / `MalformedToken` → `AppError::Unauthorized`
/// - `InvalidToken` → `AppError::Unauthorized` (valid format, rejected by Profile API)
/// - `ServiceUnavailable` → `AppError::DependencyUnavailable` (circuit breaker open or Profile API down)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailure {
    MissingToken,
    MalformedToken,
    InvalidToken,
    ServiceUnavailable,
}

/// Extract the token from a `Bearer <token>` Authorization header value.
pub fn parse_bearer_token(header: &str) -> Result<&str, AuthFailure> {
    let token = header
        .strip_prefix("Bearer ")
        .ok_or(AuthFailure::MalformedToken)?;

    if token.is_empty() {
        return Err(AuthFailure::MalformedToken);
    }

    Ok(token)
}

/// Validate a token via the Profile API, gated by the circuit breaker.
///
/// On success/auth-rejection: records a breaker success (Profile API is healthy).
/// On transport/server error: records a breaker failure (may trip the circuit).
pub async fn authenticate_token(
    token: &str,
    validator: &dyn TokenValidator,
    breaker: &CircuitBreaker,
) -> Result<AuthenticatedUser, AuthFailure> {
    if !breaker.allow_request() {
        return Err(AuthFailure::ServiceUnavailable);
    }

    match validator.validate(token).await {
        Ok(user) => {
            breaker.record_success();
            Ok(user)
        }
        Err(AppError::Unauthorized(_)) => {
            breaker.record_success();
            Err(AuthFailure::InvalidToken)
        }
        Err(AppError::DependencyUnavailable(_)) | Err(AppError::Internal(_)) => {
            breaker.record_failure();
            Err(AuthFailure::ServiceUnavailable)
        }
        Err(_) => Err(AuthFailure::ServiceUnavailable),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::clients::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};

    struct OkValidator;

    #[async_trait::async_trait]
    impl TokenValidator for OkValidator {
        async fn validate(&self, token: &str) -> Result<AuthenticatedUser, AppError> {
            Ok(AuthenticatedUser {
                user_id: uuid::Uuid::parse_str(token).expect("valid test UUID"),
                display_name: "ok".to_string(),
            })
        }
    }

    struct UnauthorizedValidator;

    #[async_trait::async_trait]
    impl TokenValidator for UnauthorizedValidator {
        async fn validate(&self, _token: &str) -> Result<AuthenticatedUser, AppError> {
            Err(AppError::Unauthorized("denied".to_string()))
        }
    }

    struct FailingValidator;

    #[async_trait::async_trait]
    impl TokenValidator for FailingValidator {
        async fn validate(&self, _token: &str) -> Result<AuthenticatedUser, AppError> {
            Err(AppError::DependencyUnavailable("profile_api".to_string()))
        }
    }

    fn test_breaker() -> Arc<CircuitBreaker> {
        Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 3,
            recovery_timeout: Duration::from_secs(30),
            success_threshold: 2,
            service_name: "test".to_string(),
            rate_window: Duration::from_secs(30),
            failure_rate_threshold: 0.5,
            min_calls_for_rate: 10,
        }))
    }

    #[test]
    fn parse_bearer_token_rejects_empty_value() {
        let err = parse_bearer_token("Bearer ").expect_err("empty token must fail");
        assert_eq!(err, AuthFailure::MalformedToken);
    }

    #[tokio::test]
    async fn authenticate_token_returns_user_on_success() {
        let validator = OkValidator;
        let breaker = test_breaker();
        let token = "a1a2a3a4-b1b2-c1c2-d1d2-e1e2e3e4e5e6";

        let user = authenticate_token(token, &validator, &breaker)
            .await
            .expect("auth succeeds");

        assert_eq!(user.user_id.to_string(), token);
    }

    #[tokio::test]
    async fn authenticate_token_maps_unauthorized_to_invalid_token() {
        let validator = UnauthorizedValidator;
        let breaker = test_breaker();

        let err = authenticate_token("token", &validator, &breaker)
            .await
            .expect_err("auth rejection must fail");

        assert_eq!(err, AuthFailure::InvalidToken);
        assert!(
            breaker.allow_request(),
            "breaker stays closed on auth rejection"
        );
    }

    #[tokio::test]
    async fn authenticate_token_maps_dependency_error_to_service_unavailable() {
        let validator = FailingValidator;
        let breaker = test_breaker();

        let err = authenticate_token("token", &validator, &breaker)
            .await
            .expect_err("dependency failure must fail");

        assert_eq!(err, AuthFailure::ServiceUnavailable);
    }
}
