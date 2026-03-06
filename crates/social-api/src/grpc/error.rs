//! Mapping from `AppError` to `tonic::Status` for gRPC error responses.
//!
//! Every gRPC service method converts domain errors via `IntoStatus` or
//! `app_error_to_status` so that clients receive idiomatic gRPC status codes.

use shared::errors::AppError;
use tonic::Status;

/// Convert an `AppError` to a `tonic::Status` with the appropriate gRPC code.
pub fn app_error_to_status(err: &AppError) -> Status {
    match err {
        AppError::Unauthorized(msg) => Status::unauthenticated(msg.clone()),

        AppError::ContentNotFound {
            content_type,
            content_id,
        } => Status::not_found(format!("Content not found: {content_type}:{content_id}")),

        AppError::ContentTypeUnknown(ct) => {
            Status::invalid_argument(format!("Unknown content type: {ct}"))
        }

        AppError::InvalidContentId(id) => {
            Status::invalid_argument(format!("Invalid content ID: {id}"))
        }

        AppError::InvalidCursor(msg) => Status::invalid_argument(format!("Invalid cursor: {msg}")),

        AppError::InvalidWindow(w) => Status::invalid_argument(format!(
            "Invalid time window: {w}. Valid values: 24h, 7d, 30d, all"
        )),

        AppError::BatchTooLarge { size, max } => {
            Status::invalid_argument(format!("Batch size {size} exceeds maximum of {max}"))
        }

        AppError::RateLimited { retry_after_secs } => Status::resource_exhausted(format!(
            "Rate limit exceeded. Retry after {retry_after_secs}s"
        )),

        AppError::DependencyUnavailable(svc) => {
            Status::unavailable(format!("External service unavailable: {svc}"))
        }

        AppError::Internal(msg) => Status::internal(msg.clone()),

        AppError::Database(msg) => Status::internal(format!("Database error: {msg}")),

        AppError::Cache(msg) => Status::internal(format!("Cache error: {msg}")),
    }
}

/// Ergonomic conversion trait for `Result<T, AppError>` -> `Result<T, tonic::Status>`.
///
/// Usage:
/// ```ignore
/// let count = like_service.get_count(&content_type, content_id).await.into_status()?;
/// ```
pub trait IntoStatus<T> {
    #[allow(clippy::result_large_err)]
    fn into_status(self) -> Result<T, Status>;
}

impl<T> IntoStatus<T> for Result<T, AppError> {
    #[allow(clippy::result_large_err)]
    fn into_status(self) -> Result<T, Status> {
        self.map_err(|e| app_error_to_status(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Code;

    #[test]
    fn unauthorized_maps_to_unauthenticated() {
        let err = AppError::Unauthorized("bad token".to_string());
        let status = app_error_to_status(&err);
        assert_eq!(status.code(), Code::Unauthenticated);
        assert!(status.message().contains("bad token"));
    }

    #[test]
    fn content_not_found_maps_to_not_found() {
        let err = AppError::ContentNotFound {
            content_type: "post".to_string(),
            content_id: "abc-123".to_string(),
        };
        let status = app_error_to_status(&err);
        assert_eq!(status.code(), Code::NotFound);
        assert!(status.message().contains("post:abc-123"));
    }

    #[test]
    fn content_type_unknown_maps_to_invalid_argument() {
        let err = AppError::ContentTypeUnknown("unknown_type".to_string());
        let status = app_error_to_status(&err);
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(status.message().contains("unknown_type"));
    }

    #[test]
    fn invalid_content_id_maps_to_invalid_argument() {
        let err = AppError::InvalidContentId("not-a-uuid".to_string());
        let status = app_error_to_status(&err);
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(status.message().contains("not-a-uuid"));
    }

    #[test]
    fn invalid_cursor_maps_to_invalid_argument() {
        let err = AppError::InvalidCursor("bad cursor".to_string());
        let status = app_error_to_status(&err);
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(status.message().contains("bad cursor"));
    }

    #[test]
    fn invalid_window_maps_to_invalid_argument() {
        let err = AppError::InvalidWindow("bad_window".to_string());
        let status = app_error_to_status(&err);
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(status.message().contains("bad_window"));
        assert!(status.message().contains("24h, 7d, 30d, all"));
    }

    #[test]
    fn batch_too_large_maps_to_invalid_argument() {
        let err = AppError::BatchTooLarge {
            size: 150,
            max: 100,
        };
        let status = app_error_to_status(&err);
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(status.message().contains("150"));
        assert!(status.message().contains("100"));
    }

    #[test]
    fn rate_limited_maps_to_resource_exhausted() {
        let err = AppError::RateLimited {
            retry_after_secs: 30,
        };
        let status = app_error_to_status(&err);
        assert_eq!(status.code(), Code::ResourceExhausted);
        assert!(status.message().contains("30s"));
    }

    #[test]
    fn dependency_unavailable_maps_to_unavailable() {
        let err = AppError::DependencyUnavailable("profile_api".to_string());
        let status = app_error_to_status(&err);
        assert_eq!(status.code(), Code::Unavailable);
        assert!(status.message().contains("profile_api"));
    }

    #[test]
    fn internal_maps_to_internal() {
        let err = AppError::Internal("something broke".to_string());
        let status = app_error_to_status(&err);
        assert_eq!(status.code(), Code::Internal);
        assert!(status.message().contains("something broke"));
    }

    #[test]
    fn database_maps_to_internal() {
        let err = AppError::Database("connection refused".to_string());
        let status = app_error_to_status(&err);
        assert_eq!(status.code(), Code::Internal);
        assert!(status.message().contains("Database error"));
        assert!(status.message().contains("connection refused"));
    }

    #[test]
    fn cache_maps_to_internal() {
        let err = AppError::Cache("timeout".to_string());
        let status = app_error_to_status(&err);
        assert_eq!(status.code(), Code::Internal);
        assert!(status.message().contains("Cache error"));
        assert!(status.message().contains("timeout"));
    }

    #[test]
    fn into_status_trait_ok_passthrough() {
        let result: Result<i32, AppError> = Ok(42);
        let converted = result.into_status();
        assert_eq!(converted.unwrap(), 42);
    }

    #[test]
    fn into_status_trait_err_converts() {
        let result: Result<i32, AppError> = Err(AppError::Unauthorized("denied".to_string()));
        let converted = result.into_status();
        let status = converted.unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);
        assert!(status.message().contains("denied"));
    }

    #[test]
    fn into_status_trait_all_error_variants() {
        // Verify every AppError variant converts without panicking
        let errors: Vec<AppError> = vec![
            AppError::Unauthorized("x".into()),
            AppError::ContentNotFound {
                content_type: "post".into(),
                content_id: "id".into(),
            },
            AppError::ContentTypeUnknown("x".into()),
            AppError::InvalidContentId("x".into()),
            AppError::BatchTooLarge { size: 1, max: 1 },
            AppError::InvalidCursor("x".into()),
            AppError::InvalidWindow("x".into()),
            AppError::RateLimited {
                retry_after_secs: 5,
            },
            AppError::DependencyUnavailable("x".into()),
            AppError::Internal("x".into()),
            AppError::Database("x".into()),
            AppError::Cache("x".into()),
        ];

        for err in errors {
            let result: Result<(), AppError> = Err(err);
            let status = result.into_status().unwrap_err();
            // Just verify it doesn't panic and produces a valid code
            let _ = status.code();
            assert!(!status.message().is_empty());
        }
    }
}
