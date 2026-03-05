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
        (status, Json(api_error)).into_response()
    }
}

/// Allow `?` operator to convert AppError -> ApiErrorResponse automatically.
impl From<AppError> for ApiErrorResponse {
    fn from(err: AppError) -> Self {
        Self(err)
    }
}
