use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use shared::errors::AppError;
use shared::types::*;
use uuid::Uuid;

use crate::errors::ApiErrorResponse;
use crate::extractors::auth::AuthUser;
use crate::extractors::content_path::ContentPath;
use crate::state::AppState;

/// POST /v1/likes — Like content
pub async fn like_content(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<LikeRequest>,
) -> Result<impl IntoResponse, ApiErrorResponse> {
    let response = state
        .like_service()
        .like(user.user_id, &body.content_type, body.content_id)
        .await?;
    Ok((StatusCode::CREATED, Json(response)))
}

/// DELETE /v1/likes/:content_type/:content_id — Unlike content
pub async fn unlike_content(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    ContentPath(content_type, content_id): ContentPath,
) -> Result<impl IntoResponse, ApiErrorResponse> {
    let response = state
        .like_service()
        .unlike(user.user_id, &content_type, content_id)
        .await?;
    Ok((StatusCode::OK, Json(response)))
}

/// GET /v1/likes/:content_type/:content_id/count — Get like count (public)
pub async fn get_count(
    State(state): State<AppState>,
    ContentPath(content_type, content_id): ContentPath,
) -> Result<impl IntoResponse, ApiErrorResponse> {
    let response = state
        .like_service()
        .get_count(&content_type, content_id)
        .await?;
    Ok((StatusCode::OK, Json(response)))
}

/// GET /v1/likes/:content_type/:content_id/status — Get like status (auth)
pub async fn get_status(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    ContentPath(content_type, content_id): ContentPath,
) -> Result<impl IntoResponse, ApiErrorResponse> {
    let response = state
        .like_service()
        .get_status(user.user_id, &content_type, content_id)
        .await?;
    Ok((StatusCode::OK, Json(response)))
}

/// GET /v1/likes/user — Get user's liked items (auth, paginated)
pub async fn get_user_likes(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(params): Query<PaginationParams>,
) -> Result<impl IntoResponse, ApiErrorResponse> {
    let limit = params.limit.unwrap_or(20);
    let response = state
        .like_service()
        .get_user_likes(
            user.user_id,
            params.content_type.as_deref(),
            params.cursor.as_deref(),
            limit,
        )
        .await?;
    Ok((StatusCode::OK, Json(response)))
}

/// POST /v1/likes/batch/counts — Batch like counts (public)
pub async fn batch_counts(
    State(state): State<AppState>,
    Json(body): Json<BatchRequest>,
) -> Result<impl IntoResponse, ApiErrorResponse> {
    let items: Vec<(String, Uuid)> = body
        .items
        .into_iter()
        .map(|i| (i.content_type, i.content_id))
        .collect();
    let results = state.like_service().batch_counts(&items).await?;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "results": results })),
    ))
}

/// POST /v1/likes/batch/statuses — Batch like statuses (auth)
pub async fn batch_statuses(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<BatchRequest>,
) -> Result<impl IntoResponse, ApiErrorResponse> {
    let items: Vec<(String, Uuid)> = body
        .items
        .into_iter()
        .map(|i| (i.content_type, i.content_id))
        .collect();
    let results = state
        .like_service()
        .batch_statuses(user.user_id, &items)
        .await?;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "results": results })),
    ))
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
) -> Result<impl IntoResponse, ApiErrorResponse> {
    let window_str = params.window.as_deref().unwrap_or("all");
    let window = TimeWindow::from_str_value(window_str)
        .ok_or_else(|| AppError::InvalidWindow(window_str.to_string()))?;
    let limit = params.limit.unwrap_or(10);
    let response = state
        .like_service()
        .get_leaderboard(params.content_type.as_deref(), window, limit)
        .await?;
    Ok((StatusCode::OK, Json(response)))
}
