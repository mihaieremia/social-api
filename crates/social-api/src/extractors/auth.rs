use axum::{
    extract::{FromRequestParts, State},
    http::{StatusCode, request::Parts},
    response::{IntoResponse, Json},
};
use shared::errors::{ApiError, ErrorCode};
use shared::types::AuthenticatedUser;
use uuid::Uuid;

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

        // Validate token via Profile API
        let url = format!("{}/v1/auth/validate", app_state.config().profile_api_url);
        let response = app_state
            .http_client()
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|_| AuthError::ServiceUnavailable)?;

        if !response.status().is_success() {
            return Err(AuthError::InvalidToken);
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|_| AuthError::ServiceUnavailable)?;

        let valid = body.get("valid").and_then(|v| v.as_bool()).unwrap_or(false);
        if !valid {
            return Err(AuthError::InvalidToken);
        }

        let user_id_str = body
            .get("user_id")
            .and_then(|v| v.as_str())
            .ok_or(AuthError::ServiceUnavailable)?;

        let uuid_str = user_id_str.strip_prefix("usr_").unwrap_or(user_id_str);
        let user_id =
            Uuid::parse_str(uuid_str).map_err(|_| AuthError::ServiceUnavailable)?;

        let display_name = body
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();

        Ok(AuthUser(AuthenticatedUser {
            user_id,
            display_name,
        }))
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
            Self::MalformedToken => (ErrorCode::Unauthorized, "Malformed authorization header. Expected: Bearer <token>"),
            Self::InvalidToken => (ErrorCode::Unauthorized, "Invalid or expired token"),
            Self::ServiceUnavailable => (ErrorCode::DependencyUnavailable, "Authentication service unavailable"),
        };

        let api_error = ApiError::new(code, message, "unknown");
        let status = StatusCode::from_u16(api_error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        (status, Json(api_error)).into_response()
    }
}
