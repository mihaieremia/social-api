pub mod health;
pub mod likes;
pub mod metrics_handler;
pub mod stream;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Json},
};
use shared::errors::AppError;

/// Newtype wrapper to implement IntoResponse for AppError (orphan rule).
/// Handlers return `Result<impl IntoResponse, ApiErrorResponse>`.
pub struct ApiErrorResponse(pub AppError);

impl IntoResponse for ApiErrorResponse {
    fn into_response(self) -> axum::response::Response {
        let api_error = self.0.to_api_error("unknown");
        let status = StatusCode::from_u16(api_error.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        // Log all 500s at error level. AppError::internal() already logs the
        // origin backtrace; this catches Database/Cache variants and any 500
        // not created via AppError::internal().
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(
                error = %self.0,
                error_code = %self.0.error_code(),
                "Responding with 500 INTERNAL_ERROR"
            );
        }

        (status, Json(api_error)).into_response()
    }
}

/// Allow `?` operator to convert AppError -> ApiErrorResponse automatically.
impl From<AppError> for ApiErrorResponse {
    fn from(err: AppError) -> Self {
        Self(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    #[test]
    fn into_response_returns_500_for_database_error() {
        // Exercises the `if status == INTERNAL_SERVER_ERROR` logging branch.
        let err = AppError::Database("connection lost".to_string());
        let response = ApiErrorResponse(err).into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn from_app_error_wraps_and_returns_401() {
        let err = AppError::Unauthorized("bad token".to_string());
        let response = ApiErrorResponse::from(err).into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
