use serde::Serialize;
use std::fmt;
use utoipa::ToSchema;

/// Error codes matching the API specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub enum ErrorCode {
    #[serde(rename = "UNAUTHORIZED")]
    Unauthorized,
    #[serde(rename = "CONTENT_NOT_FOUND")]
    ContentNotFound,
    #[serde(rename = "CONTENT_TYPE_UNKNOWN")]
    ContentTypeUnknown,
    #[serde(rename = "INVALID_CONTENT_ID")]
    InvalidContentId,
    #[serde(rename = "BATCH_TOO_LARGE")]
    BatchTooLarge,
    #[serde(rename = "INVALID_CURSOR")]
    InvalidCursor,
    #[serde(rename = "INVALID_WINDOW")]
    InvalidWindow,
    #[serde(rename = "RATE_LIMITED")]
    RateLimited,
    #[serde(rename = "DEPENDENCY_UNAVAILABLE")]
    DependencyUnavailable,
    #[serde(rename = "INTERNAL_ERROR")]
    InternalError,
}

impl ErrorCode {
    /// HTTP status code for this error.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::ContentNotFound => 404,
            Self::ContentTypeUnknown => 400,
            Self::InvalidContentId => 400,
            Self::BatchTooLarge => 400,
            Self::InvalidCursor => 400,
            Self::InvalidWindow => 400,
            Self::RateLimited => 429,
            Self::DependencyUnavailable => 503,
            Self::InternalError => 500,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Unauthorized => "UNAUTHORIZED",
            Self::ContentNotFound => "CONTENT_NOT_FOUND",
            Self::ContentTypeUnknown => "CONTENT_TYPE_UNKNOWN",
            Self::InvalidContentId => "INVALID_CONTENT_ID",
            Self::BatchTooLarge => "BATCH_TOO_LARGE",
            Self::InvalidCursor => "INVALID_CURSOR",
            Self::InvalidWindow => "INVALID_WINDOW",
            Self::RateLimited => "RATE_LIMITED",
            Self::DependencyUnavailable => "DEPENDENCY_UNAVAILABLE",
            Self::InternalError => "INTERNAL_ERROR",
        };
        f.write_str(s)
    }
}

/// Structured API error matching the specification's error envelope.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApiError {
    pub error: ApiErrorBody,
}

/// Inner error body.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApiErrorBody {
    /// Machine-readable error code
    pub code: ErrorCode,
    /// Human-readable error message
    pub message: String,
    /// Unique request identifier for tracing
    #[schema(example = "req_a1b2c3d4")]
    pub request_id: String,
    /// Additional context about the error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ApiError {
    /// Create a new API error.
    pub fn new(code: ErrorCode, message: impl Into<String>, request_id: impl Into<String>) -> Self {
        Self {
            error: ApiErrorBody {
                code,
                message: message.into(),
                request_id: request_id.into(),
                details: None,
            },
        }
    }

    /// Add details to the error.
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.error.details = Some(details);
        self
    }

    /// HTTP status code for this error.
    pub fn http_status(&self) -> u16 {
        self.error.code.http_status()
    }
}

/// Application-level error type used throughout the service layer.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Content not found: {content_type}:{content_id}")]
    ContentNotFound {
        content_type: String,
        content_id: String,
    },

    #[error("Unknown content type: {0}")]
    ContentTypeUnknown(String),

    #[error("Invalid content ID: {0}")]
    InvalidContentId(String),

    #[error("Batch too large: {size} items (max {max})")]
    BatchTooLarge { size: usize, max: usize },

    #[error("Invalid cursor: {0}")]
    InvalidCursor(String),

    #[error("Invalid time window: {0}")]
    InvalidWindow(String),

    #[error("Rate limited")]
    RateLimited { retry_after_secs: u64 },

    #[error("Dependency unavailable: {0}")]
    DependencyUnavailable(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Cache error: {0}")]
    Cache(String),
}

impl AppError {
    /// Creates an `Internal` error and immediately logs it with a backtrace at the call site.
    ///
    /// Prefer this over `AppError::Internal(msg.into())` at error-origin sites so the
    /// backtrace points to where the problem was discovered, not the response layer.
    /// Requires `RUST_BACKTRACE=1` (or `RUST_LIB_BACKTRACE=1`) at runtime for a real trace.
    pub fn internal(msg: impl std::fmt::Display) -> Self {
        let msg_str = msg.to_string();
        let bt = std::backtrace::Backtrace::capture();
        tracing::error!(
            error = %msg_str,
            backtrace = %bt,
            "Internal error at origin"
        );
        Self::Internal(msg_str)
    }

    /// Convert to an API error with the given request ID.
    pub fn to_api_error(&self, request_id: &str) -> ApiError {
        let (code, message, details) = match self {
            Self::Unauthorized(msg) => (ErrorCode::Unauthorized, msg.clone(), None),
            Self::ContentNotFound {
                content_type,
                content_id,
            } => (
                ErrorCode::ContentNotFound,
                "Content item does not exist or has been removed".to_string(),
                Some(serde_json::json!({
                    "content_type": content_type,
                    "content_id": content_id,
                })),
            ),
            Self::ContentTypeUnknown(ct) => (
                ErrorCode::ContentTypeUnknown,
                format!("Unknown content type: {ct}"),
                Some(serde_json::json!({ "content_type": ct })),
            ),
            Self::InvalidContentId(id) => (
                ErrorCode::InvalidContentId,
                format!("Invalid content ID: {id}"),
                None,
            ),
            Self::BatchTooLarge { size, max } => (
                ErrorCode::BatchTooLarge,
                format!("Batch size {size} exceeds maximum of {max}"),
                Some(serde_json::json!({ "size": size, "max": max })),
            ),
            Self::InvalidCursor(msg) => (ErrorCode::InvalidCursor, msg.clone(), None),
            Self::InvalidWindow(w) => (
                ErrorCode::InvalidWindow,
                format!("Invalid time window: {w}. Valid values: 24h, 7d, 30d, all"),
                None,
            ),
            Self::RateLimited { .. } => (
                ErrorCode::RateLimited,
                "Rate limit exceeded".to_string(),
                None,
            ),
            Self::DependencyUnavailable(svc) => (
                ErrorCode::DependencyUnavailable,
                format!("External service unavailable: {svc}"),
                None,
            ),
            Self::Internal(msg) => (ErrorCode::InternalError, msg.clone(), None),
            Self::Database(_) => (
                ErrorCode::InternalError,
                "An internal error occurred. Please try again later.".to_string(),
                None,
            ),
            Self::Cache(_) => (ErrorCode::InternalError, "Cache error".to_string(), None),
        };

        let mut err = ApiError::new(code, message, request_id);
        if let Some(d) = details {
            err = err.with_details(d);
        }
        err
    }

    /// Get the ErrorCode for this error.
    pub fn error_code(&self) -> ErrorCode {
        match self {
            Self::Unauthorized(_) => ErrorCode::Unauthorized,
            Self::ContentNotFound { .. } => ErrorCode::ContentNotFound,
            Self::ContentTypeUnknown(_) => ErrorCode::ContentTypeUnknown,
            Self::InvalidContentId(_) => ErrorCode::InvalidContentId,
            Self::BatchTooLarge { .. } => ErrorCode::BatchTooLarge,
            Self::InvalidCursor(_) => ErrorCode::InvalidCursor,
            Self::InvalidWindow(_) => ErrorCode::InvalidWindow,
            Self::RateLimited { .. } => ErrorCode::RateLimited,
            Self::DependencyUnavailable(_) => ErrorCode::DependencyUnavailable,
            Self::Internal(_) | Self::Database(_) | Self::Cache(_) => ErrorCode::InternalError,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_code_http_status_mapping() {
        assert_eq!(ErrorCode::Unauthorized.http_status(), 401);
        assert_eq!(ErrorCode::ContentNotFound.http_status(), 404);
        assert_eq!(ErrorCode::ContentTypeUnknown.http_status(), 400);
        assert_eq!(ErrorCode::InvalidContentId.http_status(), 400);
        assert_eq!(ErrorCode::BatchTooLarge.http_status(), 400);
        assert_eq!(ErrorCode::InvalidCursor.http_status(), 400);
        assert_eq!(ErrorCode::InvalidWindow.http_status(), 400);
        assert_eq!(ErrorCode::RateLimited.http_status(), 429);
        assert_eq!(ErrorCode::DependencyUnavailable.http_status(), 503);
        assert_eq!(ErrorCode::InternalError.http_status(), 500);
    }

    #[test]
    fn test_app_error_to_api_error() {
        let err = AppError::ContentNotFound {
            content_type: "post".to_string(),
            content_id: "abc-123".to_string(),
        };
        let api_err = err.to_api_error("req_1");
        assert_eq!(api_err.error.code, ErrorCode::ContentNotFound);
        assert_eq!(api_err.error.request_id, "req_1");
        assert!(api_err.error.details.is_some());
    }

    #[test]
    fn test_app_error_batch_too_large() {
        let err = AppError::BatchTooLarge {
            size: 150,
            max: 100,
        };
        assert_eq!(err.error_code(), ErrorCode::BatchTooLarge);
        let api_err = err.to_api_error("req_2");
        assert_eq!(api_err.http_status(), 400);
    }

    #[test]
    fn test_app_error_rate_limited() {
        let err = AppError::RateLimited {
            retry_after_secs: 30,
        };
        assert_eq!(err.error_code(), ErrorCode::RateLimited);
        assert_eq!(err.to_api_error("req_3").http_status(), 429);
    }

    #[test]
    fn test_api_error_with_details() {
        let err = ApiError::new(ErrorCode::InternalError, "test", "req_1")
            .with_details(serde_json::json!({"key": "value"}));
        assert!(err.error.details.is_some());
    }

    #[test]
    fn test_api_error_serialization() {
        let err = ApiError::new(ErrorCode::Unauthorized, "bad token", "req_1");
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("UNAUTHORIZED"));
        assert!(json.contains("bad token"));
        assert!(json.contains("req_1"));
    }

    #[test]
    fn test_database_and_cache_errors_map_to_internal() {
        assert_eq!(
            AppError::Database("conn failed".into()).error_code(),
            ErrorCode::InternalError
        );
        assert_eq!(
            AppError::Cache("timeout".into()).error_code(),
            ErrorCode::InternalError
        );
    }

    #[test]
    fn test_database_error_does_not_leak_internals() {
        let err = AppError::Database(
            "duplicate key value violates unique constraint \"uq_likes_user_content\"".into(),
        );
        let api_err = err.to_api_error("req_x");
        assert!(!api_err.error.message.contains("uq_likes_user_content"));
        assert!(!api_err.error.message.contains("duplicate key"));
        assert_eq!(api_err.error.code, ErrorCode::InternalError);
    }
}
