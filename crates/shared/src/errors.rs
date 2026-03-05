use serde::Serialize;
use std::fmt;

/// Error codes matching the API specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
pub struct ApiError {
    pub error: ApiErrorBody,
}

/// Inner error body.
#[derive(Debug, Clone, Serialize)]
pub struct ApiErrorBody {
    pub code: ErrorCode,
    pub message: String,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ApiError {
    /// Create a new API error.
    pub fn new(
        code: ErrorCode,
        message: impl Into<String>,
        request_id: impl Into<String>,
    ) -> Self {
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
            Self::Cache(_) => (
                ErrorCode::InternalError,
                "Cache error".to_string(),
                None,
            ),
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
