use axum::{
    extract::FromRequestParts,
    http::{StatusCode, request::Parts},
    response::{IntoResponse, Json},
};
use shared::errors::{ApiError, AppError, ErrorCode};
use shared::types::AuthenticatedUser;

use crate::clients::profile_client::TokenValidator;
use crate::state::AppState;

/// Extractor that validates Bearer token and provides AuthenticatedUser.
/// Usage: `async fn handler(AuthUser(user): AuthUser) -> ...`
pub struct AuthUser(pub AuthenticatedUser);

impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);

        // Extract Bearer token from Authorization header
        let auth_header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or(AuthError::MissingToken)?;

        let token = auth_header
            .strip_prefix("Bearer ")
            .ok_or(AuthError::MalformedToken)?;

        if token.is_empty() {
            return Err(AuthError::MalformedToken);
        }

        // Check circuit breaker before calling Profile API
        if !app_state.profile_breaker().allow_request() {
            return Err(AuthError::ServiceUnavailable);
        }

        // Delegate to TokenValidator trait implementation
        match app_state.token_validator().validate(token).await {
            Ok(user) => {
                app_state.profile_breaker().record_success();
                // Record authenticated user_id into the active TraceLayer span.
                tracing::Span::current().record("user_id", user.user_id.to_string());
                Ok(AuthUser(user))
            }
            Err(e) => {
                // Record failure for dependency errors, not auth errors
                match &e {
                    AppError::DependencyUnavailable(_) | AppError::Internal(_) => {
                        app_state.profile_breaker().record_failure();
                        Err(AuthError::ServiceUnavailable)
                    }
                    AppError::Unauthorized(_) => Err(AuthError::InvalidToken),
                    _ => Err(AuthError::ServiceUnavailable),
                }
            }
        }
    }
}

/// Helper trait for extracting AppState from composite state types.
pub trait FromRef<T> {
    fn from_ref(input: &T) -> Self;
}

impl FromRef<AppState> for AppState {
    fn from_ref(input: &AppState) -> Self {
        input.clone()
    }
}

/// Auth rejection error type.
#[derive(Debug)]
pub enum AuthError {
    MissingToken,
    MalformedToken,
    InvalidToken,
    ServiceUnavailable,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> axum::response::Response {
        let (code, message) = match self {
            Self::MissingToken => (ErrorCode::Unauthorized, "Missing authorization header"),
            Self::MalformedToken => (
                ErrorCode::Unauthorized,
                "Malformed authorization header. Expected: Bearer <token>",
            ),
            Self::InvalidToken => (ErrorCode::Unauthorized, "Invalid or expired token"),
            Self::ServiceUnavailable => (
                ErrorCode::DependencyUnavailable,
                "Authentication service unavailable",
            ),
        };

        let api_error = ApiError::new(code, message, "unknown");
        let status = StatusCode::from_u16(api_error.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        (status, Json(api_error)).into_response()
    }
}
