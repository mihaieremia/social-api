//! gRPC authentication helpers.
//!
//! Extracts bearer tokens from gRPC metadata and validates them via the
//! `TokenValidator` trait + circuit breaker, mirroring the HTTP `AuthUser`
//! extractor logic but adapted for tonic's async service method context.

// Building block for gRPC service implementations (Task 8+).
#![allow(dead_code)]

use shared::errors::AppError;
use shared::types::AuthenticatedUser;

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

    let token = auth
        .strip_prefix("Bearer ")
        .ok_or_else(|| tonic::Status::unauthenticated("Invalid authorization format"))?;

    Ok(token.to_string())
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
    if !breaker.allow_request() {
        return Err(tonic::Status::unavailable(
            "Profile service circuit breaker is open",
        ));
    }

    match validator.validate(token).await {
        Ok(user) => {
            breaker.record_success();
            Ok(user)
        }
        Err(AppError::Unauthorized(msg)) => {
            // Auth rejection is a successful call to the profile service
            breaker.record_success();
            Err(tonic::Status::unauthenticated(msg))
        }
        Err(AppError::DependencyUnavailable(svc)) => {
            breaker.record_failure();
            Err(tonic::Status::unavailable(format!(
                "External service unavailable: {svc}"
            )))
        }
        Err(e) => {
            breaker.record_failure();
            Err(tonic::Status::internal(e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

        // "Bearer " is valid prefix; empty token is returned (validator will reject it)
        let token = extract_token(&metadata).unwrap();
        assert!(token.is_empty());
    }
}
