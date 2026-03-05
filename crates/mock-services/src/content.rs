use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde_json::json;
use uuid::Uuid;

use crate::data;

/// GET /v1/:content_type/:content_id
/// Validates that a content item exists.
pub async fn get_content(
    Path((content_type, content_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let id = match Uuid::parse_str(&content_id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid_content_id" })),
            )
                .into_response();
        }
    };

    let valid_ids = data::content_ids(&content_type);

    if valid_ids.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown_content_type" })),
        )
            .into_response();
    }

    if valid_ids.contains(&id) {
        (
            StatusCode::OK,
            Json(json!({
                "id": id,
                "title": format!("Mock {} item {}", content_type, id),
                "content_type": content_type,
            })),
        )
            .into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "content_not_found" })),
        )
            .into_response()
    }
}

/// GET /health
/// Health check for mock service.
pub async fn health() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}
