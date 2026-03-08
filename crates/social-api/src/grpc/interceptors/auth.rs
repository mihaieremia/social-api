//! gRPC authentication helpers built on the shared auth flow.

use shared::types::AuthenticatedUser;

use crate::auth::{self as authentication, AuthFailure};
use crate::clients::circuit_breaker::CircuitBreaker;
use crate::clients::profile_client::TokenValidator;

/// Extract a bearer token from gRPC request metadata.
///
/// Expects an `authorization` metadata key with value `Bearer <token>`.
/// Returns the raw token string (without the `Bearer ` prefix).
#[allow(clippy::result_large_err)]
pub fn extract_token(metadata: &tonic::metadata::MetadataMap) -> Result<String, tonic::Status> {
    let auth = metadata
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| tonic::Status::unauthenticated("Missing authorization metadata"))?;

    authentication::parse_bearer_token(auth)
        .map(str::to_string)
        .map_err(auth_failure_to_status)
}

/// Validate a bearer token through the profile service with circuit breaker
/// protection.
///
/// Maps `AppError` variants to appropriate `tonic::Status` codes:
/// - `Unauthorized` -> `UNAUTHENTICATED` (expected; breaker records success)
/// - `DependencyUnavailable` -> `UNAVAILABLE` (breaker records failure)
/// - Other errors -> `INTERNAL` (breaker records failure)
#[allow(clippy::result_large_err)]
pub async fn validate_token(
    token: &str,
    validator: &dyn TokenValidator,
    breaker: &CircuitBreaker,
) -> Result<AuthenticatedUser, tonic::Status> {
    authentication::authenticate_token(token, validator, breaker)
        .await
        .map_err(auth_failure_to_status)
}

fn auth_failure_to_status(error: AuthFailure) -> tonic::Status {
    match error {
        AuthFailure::MissingToken => {
            tonic::Status::unauthenticated("Missing authorization metadata")
        }
        AuthFailure::MalformedToken => {
            tonic::Status::unauthenticated("Invalid authorization format")
        }
        AuthFailure::InvalidToken => tonic::Status::unauthenticated("Invalid or expired token"),
        AuthFailure::ServiceUnavailable => {
            tonic::Status::unavailable("Authentication service unavailable")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::errors::AppError;
    use tonic::metadata::MetadataMap;

    #[test]
    fn extract_token_success() {
        let mut metadata = MetadataMap::new();
        metadata.insert("authorization", "Bearer tok_abc123".parse().unwrap());

        let token = extract_token(&metadata).unwrap();
        assert_eq!(token, "tok_abc123");
    }

    #[test]
    fn extract_token_missing_header() {
        let metadata = MetadataMap::new();
        let err = extract_token(&metadata).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert!(err.message().contains("Missing authorization"));
    }

    #[test]
    fn extract_token_wrong_prefix() {
        let mut metadata = MetadataMap::new();
        metadata.insert("authorization", "Basic dXNlcjpwYXNz".parse().unwrap());

        let err = extract_token(&metadata).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert!(err.message().contains("Invalid authorization format"));
    }

    #[test]
    fn extract_token_bearer_only_no_token() {
        let mut metadata = MetadataMap::new();
        metadata.insert("authorization", "Bearer ".parse().unwrap());

        let err = extract_token(&metadata).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    // --- validate_token tests ---

    use crate::clients::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
    use std::sync::Arc;
    use std::time::Duration;

    struct TestTokenValidator;
    #[async_trait::async_trait]
    impl TokenValidator for TestTokenValidator {
        async fn validate(&self, token: &str) -> Result<AuthenticatedUser, AppError> {
            let user_id = uuid::Uuid::parse_str(token)
                .map_err(|_| AppError::Unauthorized("bad token".into()))?;
            Ok(AuthenticatedUser {
                user_id,
                display_name: "Test".to_string(),
            })
        }
    }

    struct FailingValidator;
    #[async_trait::async_trait]
    impl TokenValidator for FailingValidator {
        async fn validate(&self, _token: &str) -> Result<AuthenticatedUser, AppError> {
            Err(AppError::DependencyUnavailable("profile_api".into()))
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

    #[tokio::test]
    async fn validate_token_success_with_valid_uuid() {
        let validator = TestTokenValidator;
        let breaker = test_breaker();
        let uuid_str = "a1a2a3a4-b1b2-c1c2-d1d2-e1e2e3e4e5e6";

        let result = validate_token(uuid_str, &validator, &breaker).await;

        let user = result.expect("should succeed with valid UUID token");
        assert_eq!(user.user_id.to_string(), uuid_str);
        assert_eq!(user.display_name, "Test");
    }

    #[tokio::test]
    async fn validate_token_unauthenticated_on_invalid_token() {
        let validator = TestTokenValidator;
        let breaker = test_breaker();

        let result = validate_token("not-a-uuid", &validator, &breaker).await;

        let err = result.expect_err("should fail with invalid token");
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn validate_token_unavailable_when_breaker_open() {
        let validator = TestTokenValidator;
        let breaker = test_breaker();

        // Trip the breaker by recording enough failures
        for _ in 0..3 {
            breaker.record_failure();
        }

        let result =
            validate_token("a1a2a3a4-b1b2-c1c2-d1d2-e1e2e3e4e5e6", &validator, &breaker).await;

        let err = result.expect_err("should fail when breaker is open");
        assert_eq!(err.code(), tonic::Code::Unavailable);
        assert!(err.message().contains("Authentication service unavailable"));
    }

    #[tokio::test]
    async fn validate_token_records_success_on_auth_rejection() {
        let validator = TestTokenValidator;
        let breaker = test_breaker();

        // Record 2 failures (one away from tripping)
        breaker.record_failure();
        breaker.record_failure();

        // Invalid token -> Unauthenticated, but this is a successful API call
        let result = validate_token("not-a-uuid", &validator, &breaker).await;
        let err = result.expect_err("should fail with invalid token");
        assert_eq!(err.code(), tonic::Code::Unauthenticated);

        // Breaker should still allow requests (record_success resets consecutive failures)
        assert!(
            breaker.allow_request(),
            "breaker should stay closed after auth rejection"
        );
    }

    #[tokio::test]
    async fn validate_token_records_failure_on_dependency_error() {
        let validator = FailingValidator;
        let breaker = test_breaker();

        let result =
            validate_token("a1a2a3a4-b1b2-c1c2-d1d2-e1e2e3e4e5e6", &validator, &breaker).await;

        let err = result.expect_err("should fail when dependency is unavailable");
        assert_eq!(err.code(), tonic::Code::Unavailable);
        assert!(err.message().contains("Authentication service unavailable"));

        // Verify the failure was recorded on the breaker (2 more failures should trip it)
        breaker.record_failure();
        breaker.record_failure();
        assert!(
            !breaker.allow_request(),
            "breaker should be open after 3 total failures"
        );
    }
}
