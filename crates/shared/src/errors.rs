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
            Self::Database(msg) => (
                ErrorCode::InternalError,
                format!("Database error: {msg}"),
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
    fn test_error_code_display() {
        assert_eq!(ErrorCode::Unauthorized.to_string(), "UNAUTHORIZED");
        assert_eq!(ErrorCode::ContentNotFound.to_string(), "CONTENT_NOT_FOUND");
        assert_eq!(
            ErrorCode::ContentTypeUnknown.to_string(),
            "CONTENT_TYPE_UNKNOWN"
        );
        assert_eq!(
            ErrorCode::InvalidContentId.to_string(),
            "INVALID_CONTENT_ID"
        );
        assert_eq!(ErrorCode::BatchTooLarge.to_string(), "BATCH_TOO_LARGE");
        assert_eq!(ErrorCode::InvalidCursor.to_string(), "INVALID_CURSOR");
        assert_eq!(ErrorCode::InvalidWindow.to_string(), "INVALID_WINDOW");
        assert_eq!(ErrorCode::RateLimited.to_string(), "RATE_LIMITED");
        assert_eq!(
            ErrorCode::DependencyUnavailable.to_string(),
            "DEPENDENCY_UNAVAILABLE"
        );
        assert_eq!(ErrorCode::InternalError.to_string(), "INTERNAL_ERROR");
    }

    #[test]
    fn test_all_app_error_variants_to_api_error() {
        // Unauthorized
        let err = AppError::Unauthorized("bad token".to_string());
        let api = err.to_api_error("req-1");
        assert_eq!(api.error.code, ErrorCode::Unauthorized);
        assert_eq!(api.http_status(), 401);
        assert!(api.error.details.is_none());

        // ContentTypeUnknown
        let err = AppError::ContentTypeUnknown("unknown_type".to_string());
        let api = err.to_api_error("req-2");
        assert_eq!(api.error.code, ErrorCode::ContentTypeUnknown);
        assert_eq!(api.http_status(), 400);
        assert!(api.error.details.is_some());

        // InvalidContentId
        let err = AppError::InvalidContentId("not-a-uuid".to_string());
        let api = err.to_api_error("req-3");
        assert_eq!(api.error.code, ErrorCode::InvalidContentId);
        assert_eq!(api.http_status(), 400);
        assert!(api.error.details.is_none());

        // InvalidCursor
        let err = AppError::InvalidCursor("bad cursor".to_string());
        let api = err.to_api_error("req-4");
        assert_eq!(api.error.code, ErrorCode::InvalidCursor);
        assert_eq!(api.http_status(), 400);

        // InvalidWindow
        let err = AppError::InvalidWindow("bad_window".to_string());
        let api = err.to_api_error("req-5");
        assert_eq!(api.error.code, ErrorCode::InvalidWindow);
        assert_eq!(api.http_status(), 400);

        // DependencyUnavailable
        let err = AppError::DependencyUnavailable("profile_api".to_string());
        let api = err.to_api_error("req-6");
        assert_eq!(api.error.code, ErrorCode::DependencyUnavailable);
        assert_eq!(api.http_status(), 503);

        // Internal
        let err = AppError::Internal("something failed".to_string());
        let api = err.to_api_error("req-7");
        assert_eq!(api.error.code, ErrorCode::InternalError);
        assert_eq!(api.http_status(), 500);

        // Database
        let err = AppError::Database("connection refused".to_string());
        let api = err.to_api_error("req-8");
        assert_eq!(api.error.code, ErrorCode::InternalError);
        assert!(api.error.message.contains("Database error"));

        // Cache
        let err = AppError::Cache("timeout".to_string());
        let api = err.to_api_error("req-9");
        assert_eq!(api.error.code, ErrorCode::InternalError);
        assert!(api.error.message.contains("Cache error"));
    }

    #[test]
    fn test_all_app_error_variants_error_code() {
        assert_eq!(
            AppError::Unauthorized("x".into()).error_code(),
            ErrorCode::Unauthorized
        );
        assert_eq!(
            AppError::ContentNotFound {
                content_type: "post".into(),
                content_id: "id".into()
            }
            .error_code(),
            ErrorCode::ContentNotFound
        );
        assert_eq!(
            AppError::ContentTypeUnknown("x".into()).error_code(),
            ErrorCode::ContentTypeUnknown
        );
        assert_eq!(
            AppError::InvalidContentId("x".into()).error_code(),
            ErrorCode::InvalidContentId
        );
        assert_eq!(
            AppError::BatchTooLarge { size: 1, max: 1 }.error_code(),
            ErrorCode::BatchTooLarge
        );
        assert_eq!(
            AppError::InvalidCursor("x".into()).error_code(),
            ErrorCode::InvalidCursor
        );
        assert_eq!(
            AppError::InvalidWindow("x".into()).error_code(),
            ErrorCode::InvalidWindow
        );
        assert_eq!(
            AppError::RateLimited { retry_after_secs: 5 }.error_code(),
            ErrorCode::RateLimited
        );
        assert_eq!(
            AppError::DependencyUnavailable("x".into()).error_code(),
            ErrorCode::DependencyUnavailable
        );
        assert_eq!(
            AppError::Internal("x".into()).error_code(),
            ErrorCode::InternalError
        );
    }

    #[test]
    fn test_app_error_display_messages() {
        let e = AppError::Unauthorized("bad token".to_string());
        assert!(e.to_string().contains("Unauthorized"));

        let e = AppError::ContentNotFound {
            content_type: "post".to_string(),
            content_id: "abc".to_string(),
        };
        assert!(e.to_string().contains("Content not found"));
        assert!(e.to_string().contains("post:abc"));

        let e = AppError::ContentTypeUnknown("foo".to_string());
        assert!(e.to_string().contains("Unknown content type"));
        assert!(e.to_string().contains("foo"));

        let e = AppError::InvalidContentId("bad-id".to_string());
        assert!(e.to_string().contains("Invalid content ID"));

        let e = AppError::BatchTooLarge { size: 150, max: 100 };
        assert!(e.to_string().contains("150"));
        assert!(e.to_string().contains("100"));

        let e = AppError::InvalidCursor("bad".to_string());
        assert!(e.to_string().contains("Invalid cursor"));

        let e = AppError::InvalidWindow("bad".to_string());
        assert!(e.to_string().contains("Invalid time window"));

        let e = AppError::RateLimited { retry_after_secs: 30 };
        assert!(e.to_string().contains("Rate limited"));

        let e = AppError::DependencyUnavailable("svc".to_string());
        assert!(e.to_string().contains("Dependency unavailable"));

        let e = AppError::Internal("oops".to_string());
        assert!(e.to_string().contains("Internal error"));

        let e = AppError::Database("db error".to_string());
        assert!(e.to_string().contains("Database error"));

        let e = AppError::Cache("cache error".to_string());
        assert!(e.to_string().contains("Cache error"));
    }

    #[test]
    fn test_app_error_internal_constructor() {
        // Tests AppError::internal() — the helper that logs and captures backtrace
        let err = AppError::internal("something went wrong");
        assert!(matches!(err, AppError::Internal(_)));
        let msg = err.to_string();
        assert!(msg.contains("something went wrong"));
    }

    #[test]
    fn test_api_error_new_fields() {
        let err = ApiError::new(ErrorCode::Unauthorized, "test msg", "req-abc");
        assert_eq!(err.error.message, "test msg");
        assert_eq!(err.error.request_id, "req-abc");
        assert!(err.error.details.is_none());
        assert_eq!(err.http_status(), 401);
    }

    #[test]
    fn test_api_error_http_status_delegates_to_code() {
        let err = ApiError::new(ErrorCode::DependencyUnavailable, "svc down", "req-1");
        assert_eq!(err.http_status(), 503);

        let err = ApiError::new(ErrorCode::RateLimited, "too many", "req-2");
        assert_eq!(err.http_status(), 429);

        let err = ApiError::new(ErrorCode::InternalError, "oops", "req-3");
        assert_eq!(err.http_status(), 500);
    }
}
