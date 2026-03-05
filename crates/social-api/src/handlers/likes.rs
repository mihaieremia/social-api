use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use shared::errors::AppError;
use shared::types::*;
use uuid::Uuid;

use crate::extractors::auth::AuthUser;
use crate::state::AppState;

/// POST /v1/likes — Like content
pub async fn like_content(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<LikeRequest>,
) -> impl IntoResponse {
    match state
        .like_service()
        .like(user.user_id, &body.content_type, body.content_id)
        .await
    {
        Ok(response) => (StatusCode::CREATED, Json(response)).into_response(),
        Err(e) => error_response(e),
    }
}

/// DELETE /v1/likes/:content_type/:content_id — Unlike content
pub async fn unlike_content(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((content_type, content_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let content_id = match Uuid::parse_str(&content_id) {
        Ok(id) => id,
        Err(_) => return error_response(AppError::InvalidContentId(content_id)),
    };

    match state
        .like_service()
        .unlike(user.user_id, &content_type, content_id)
        .await
    {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(e) => error_response(e),
    }
}

/// GET /v1/likes/:content_type/:content_id/count — Get like count (public)
pub async fn get_count(
    State(state): State<AppState>,
    Path((content_type, content_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let content_id = match Uuid::parse_str(&content_id) {
        Ok(id) => id,
        Err(_) => return error_response(AppError::InvalidContentId(content_id)),
    };

    match state.like_service().get_count(&content_type, content_id).await {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(e) => error_response(e),
    }
}

/// GET /v1/likes/:content_type/:content_id/status — Get like status (auth)
pub async fn get_status(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((content_type, content_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let content_id = match Uuid::parse_str(&content_id) {
        Ok(id) => id,
        Err(_) => return error_response(AppError::InvalidContentId(content_id)),
    };

    match state
        .like_service()
        .get_status(user.user_id, &content_type, content_id)
        .await
    {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(e) => error_response(e),
    }
}

/// GET /v1/likes/user — Get user's liked items (auth, paginated)
pub async fn get_user_likes(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(params): Query<PaginationParams>,
) -> impl IntoResponse {
    let limit = params.limit.unwrap_or(20);

    match state
        .like_service()
        .get_user_likes(
            user.user_id,
            params.content_type.as_deref(),
            params.cursor.as_deref(),
            limit,
        )
        .await
    {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(e) => error_response(e),
    }
}

/// POST /v1/likes/batch/counts — Batch like counts (public)
pub async fn batch_counts(
    State(state): State<AppState>,
    Json(body): Json<BatchRequest>,
) -> impl IntoResponse {
    let items: Vec<(String, Uuid)> = body
        .items
        .into_iter()
        .map(|i| (i.content_type, i.content_id))
        .collect();

    match state.like_service().batch_counts(&items).await {
        Ok(results) => {
            (StatusCode::OK, Json(serde_json::json!({ "results": results }))).into_response()
        }
        Err(e) => error_response(e),
    }
}

/// POST /v1/likes/batch/statuses — Batch like statuses (auth)
pub async fn batch_statuses(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<BatchRequest>,
) -> impl IntoResponse {
    let items: Vec<(String, Uuid)> = body
        .items
        .into_iter()
        .map(|i| (i.content_type, i.content_id))
        .collect();

    match state
        .like_service()
        .batch_statuses(user.user_id, &items)
        .await
    {
        Ok(results) => {
            (StatusCode::OK, Json(serde_json::json!({ "results": results }))).into_response()
        }
        Err(e) => error_response(e),
    }
}

/// Query params for leaderboard.
#[derive(Debug, serde::Deserialize)]
pub struct LeaderboardParams {
    pub content_type: Option<String>,
    pub window: Option<String>,
    pub limit: Option<i64>,
}

/// GET /v1/likes/top — Top liked content (public)
pub async fn get_leaderboard(
    State(state): State<AppState>,
    Query(params): Query<LeaderboardParams>,
) -> impl IntoResponse {
    let window_str = params.window.as_deref().unwrap_or("all");
    let window = match TimeWindow::from_str_value(window_str) {
        Some(w) => w,
        None => return error_response(AppError::InvalidWindow(window_str.to_string())),
    };

    let limit = params.limit.unwrap_or(10);

    match state
        .like_service()
        .get_leaderboard(params.content_type.as_deref(), window, limit)
        .await
    {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(e) => error_response(e),
    }
}

/// Convert AppError to HTTP response.
fn error_response(err: AppError) -> axum::response::Response {
    let api_error = err.to_api_error("unknown");
    let status =
        StatusCode::from_u16(api_error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(api_error)).into_response()
}
