use axum::{
    extract::{FromRequestParts, Path},
    http::request::Parts,
};
use shared::errors::AppError;
use uuid::Uuid;

use crate::errors::ApiErrorResponse;
use crate::extractors::auth::FromRef;
use crate::state::AppState;

/// Extractor that validates content_type against the config registry and parses
/// the content_id as UUID from path parameters `{content_type}/{content_id}`.
///
/// Usage: `async fn handler(ContentPath(content_type, content_id): ContentPath) -> ...`
pub struct ContentPath(pub String, pub Uuid);

impl<S> FromRequestParts<S> for ContentPath
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = ApiErrorResponse;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);

        // Extract path params
        let Path((content_type, content_id_str)) =
            Path::<(String, String)>::from_request_parts(parts, state)
                .await
                .map_err(|_| AppError::InvalidContentId("Invalid path parameters".to_string()))?;

        // Validate content type against registry
        if !app_state.config().is_valid_content_type(&content_type) {
            return Err(AppError::ContentTypeUnknown(content_type).into());
        }

        // Parse UUID
        let content_id = Uuid::parse_str(&content_id_str)
            .map_err(|_| AppError::InvalidContentId(content_id_str))?;

        Ok(ContentPath(content_type, content_id))
    }
}
