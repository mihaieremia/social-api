use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use shared::errors::AppError;
use shared::types::*;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::errors::ApiErrorResponse;
use crate::extractors::auth::AuthUser;
use crate::extractors::content_path::ContentPath;
use crate::middleware::rate_limit;
use crate::state::AppState;

/// Validate that a content_type string is registered in the config.
/// Used by handlers that receive content_type outside of path params
/// (JSON body, query params) where ContentPath extractor doesn't apply.
fn validate_content_type(state: &AppState, content_type: &str) -> Result<(), AppError> {
    if !state.config().is_valid_content_type(content_type) {
        return Err(AppError::ContentTypeUnknown(content_type.to_string()));
    }
    Ok(())
}

/// Validate that all content types in a batch are registered.
fn validate_batch_content_types(state: &AppState, items: &[BatchItem]) -> Result<(), AppError> {
    for item in items {
        validate_content_type(state, &item.content_type)?;
    }
    Ok(())
}

/// Extract (content_type, content_id) tuples from a batch request.
fn extract_batch_items(batch: BatchRequest) -> Vec<(String, Uuid)> {
    batch
        .items
        .into_iter()
        .map(|i| (i.content_type, i.content_id))
        .collect()
}

/// POST /v1/likes — Like content
#[utoipa::path(
    post,
    path = "/v1/likes",
    request_body = LikeRequest,
    responses(
        (status = 201, description = "Content liked successfully", body = LikeActionResponse),
        (status = 400, description = "Invalid content type or ID", body = shared::errors::ApiError),
        (status = 401, description = "Unauthorized", body = shared::errors::ApiError),
        (status = 404, description = "Content not found", body = shared::errors::ApiError),
        (status = 429, description = "Rate limit exceeded", body = shared::errors::ApiError),
        (status = 503, description = "Dependency unavailable", body = shared::errors::ApiError),
    ),
    security(("bearer_auth" = [])),
    tag = "Likes"
)]
pub async fn like_content(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<LikeRequest>,
) -> Result<Response, ApiErrorResponse> {
    // Per-user write rate limit (after auth — unauthenticated gets 401 not 429)
    let rate_limit_result = rate_limit::check_user_write_limit(
        state.cache(),
        user.user_id,
        state.config().rate_limit_write_per_minute,
    )
    .await;
    if !rate_limit_result.allowed {
        return Ok(rate_limit::rate_limited_response(&rate_limit_result));
    }

    validate_content_type(&state, &body.content_type)?;

    let response = state
        .like_service()
        .like(user.user_id, &body.content_type, body.content_id)
        .await?;
    let mut http_response = (StatusCode::CREATED, Json(response)).into_response();
    rate_limit::add_rate_limit_headers(&mut http_response, &rate_limit_result);
    Ok(http_response)
}

/// DELETE /v1/likes/:content_type/:content_id — Unlike content
#[utoipa::path(
    delete,
    path = "/v1/likes/{content_type}/{content_id}",
    params(
        ("content_type" = String, Path, description = "Content type"),
        ("content_id" = Uuid, Path, description = "Content ID"),
    ),
    responses(
        (status = 200, description = "Content unliked successfully", body = LikeActionResponse),
        (status = 401, description = "Unauthorized", body = shared::errors::ApiError),
        (status = 404, description = "Content not found", body = shared::errors::ApiError),
        (status = 429, description = "Rate limit exceeded", body = shared::errors::ApiError),
    ),
    security(("bearer_auth" = [])),
    tag = "Likes"
)]
pub async fn unlike_content(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    ContentPath(content_type, content_id): ContentPath,
) -> Result<Response, ApiErrorResponse> {
    let rate_limit_result = rate_limit::check_user_write_limit(
        state.cache(),
        user.user_id,
        state.config().rate_limit_write_per_minute,
    )
    .await;
    if !rate_limit_result.allowed {
        return Ok(rate_limit::rate_limited_response(&rate_limit_result));
    }

    let response = state
        .like_service()
        .unlike(user.user_id, &content_type, content_id)
        .await?;
    let mut http_response = (StatusCode::OK, Json(response)).into_response();
    rate_limit::add_rate_limit_headers(&mut http_response, &rate_limit_result);
    Ok(http_response)
}

/// GET /v1/likes/:content_type/:content_id/count — Get like count (public)
#[utoipa::path(
    get,
    path = "/v1/likes/{content_type}/{content_id}/count",
    params(
        ("content_type" = String, Path, description = "Content type"),
        ("content_id" = Uuid, Path, description = "Content ID"),
    ),
    responses(
        (status = 200, description = "Like count retrieved", body = LikeCountResponse),
        (status = 400, description = "Invalid content type or ID", body = shared::errors::ApiError),
    ),
    tag = "Likes"
)]
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
#[utoipa::path(
    get,
    path = "/v1/likes/{content_type}/{content_id}/status",
    params(
        ("content_type" = String, Path, description = "Content type"),
        ("content_id" = Uuid, Path, description = "Content ID"),
    ),
    responses(
        (status = 200, description = "Like status retrieved", body = LikeStatusResponse),
        (status = 400, description = "Invalid content type or ID", body = shared::errors::ApiError),
        (status = 401, description = "Unauthorized", body = shared::errors::ApiError),
    ),
    security(("bearer_auth" = [])),
    tag = "Likes"
)]
pub async fn get_status(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    ContentPath(content_type, content_id): ContentPath,
) -> Result<Response, ApiErrorResponse> {
    let rate_limit_result = rate_limit::check_user_read_limit(
        state.cache(),
        user.user_id,
        state.config().rate_limit_read_per_minute,
    )
    .await;
    if !rate_limit_result.allowed {
        return Ok(rate_limit::rate_limited_response(&rate_limit_result));
    }

    let response = state
        .like_service()
        .get_status(user.user_id, &content_type, content_id)
        .await?;
    let mut http_response = (StatusCode::OK, Json(response)).into_response();
    rate_limit::add_rate_limit_headers(&mut http_response, &rate_limit_result);
    Ok(http_response)
}

/// GET /v1/likes/user — Get user's liked items (auth, paginated)
#[utoipa::path(
    get,
    path = "/v1/likes/user",
    params(
        ("cursor" = Option<String>, Query, description = "Pagination cursor"),
        ("limit" = Option<i64>, Query, description = "Max items to return"),
        ("content_type" = Option<String>, Query, description = "Filter by content type"),
    ),
    responses(
        (status = 200, description = "User likes retrieved", body = PaginatedUserLikes),
        (status = 400, description = "Invalid cursor or parameters", body = shared::errors::ApiError),
        (status = 401, description = "Unauthorized", body = shared::errors::ApiError),
    ),
    security(("bearer_auth" = [])),
    tag = "User Likes"
)]
pub async fn get_user_likes(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(params): Query<PaginationParams>,
) -> Result<Response, ApiErrorResponse> {
    let rate_limit_result = rate_limit::check_user_read_limit(
        state.cache(),
        user.user_id,
        state.config().rate_limit_read_per_minute,
    )
    .await;
    if !rate_limit_result.allowed {
        return Ok(rate_limit::rate_limited_response(&rate_limit_result));
    }

    // Validate optional content_type filter
    if let Some(ref ct) = params.content_type {
        validate_content_type(&state, ct)?;
    }

    let limit = params.limit.unwrap_or(20).clamp(1, 100);
    let response = state
        .like_service()
        .get_user_likes(
            user.user_id,
            params.content_type.as_deref(),
            params.cursor.as_deref(),
            limit,
        )
        .await?;
    let mut http_response =
        (StatusCode::OK, Json(PaginatedUserLikes::from(response))).into_response();
    rate_limit::add_rate_limit_headers(&mut http_response, &rate_limit_result);
    Ok(http_response)
}

/// POST /v1/likes/batch/counts — Batch like counts (public)
#[utoipa::path(
    post,
    path = "/v1/likes/batch/counts",
    request_body = BatchRequest,
    responses(
        (status = 200, description = "Batch counts retrieved", body = BatchCountsResponse),
        (status = 400, description = "Invalid request or batch too large", body = shared::errors::ApiError),
    ),
    tag = "Batch"
)]
pub async fn batch_counts(
    State(state): State<AppState>,
    Json(body): Json<BatchRequest>,
) -> Result<impl IntoResponse, ApiErrorResponse> {
    validate_batch_content_types(&state, &body.items)?;
    let items = extract_batch_items(body);
    let results = state.like_service().batch_counts(&items).await?;
    Ok((StatusCode::OK, Json(BatchCountsResponse { results })))
}

/// POST /v1/likes/batch/statuses — Batch like statuses (auth)
#[utoipa::path(
    post,
    path = "/v1/likes/batch/statuses",
    request_body = BatchRequest,
    responses(
        (status = 200, description = "Batch statuses retrieved", body = BatchStatusesResponse),
        (status = 400, description = "Invalid request or batch too large", body = shared::errors::ApiError),
        (status = 401, description = "Unauthorized", body = shared::errors::ApiError),
    ),
    security(("bearer_auth" = [])),
    tag = "Batch"
)]
pub async fn batch_statuses(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<BatchRequest>,
) -> Result<Response, ApiErrorResponse> {
    let rate_limit_result = rate_limit::check_user_read_limit(
        state.cache(),
        user.user_id,
        state.config().rate_limit_read_per_minute,
    )
    .await;
    if !rate_limit_result.allowed {
        return Ok(rate_limit::rate_limited_response(&rate_limit_result));
    }

    validate_batch_content_types(&state, &body.items)?;
    let items = extract_batch_items(body);
    let results = state
        .like_service()
        .batch_statuses(user.user_id, &items)
        .await?;
    let mut http_response =
        (StatusCode::OK, Json(BatchStatusesResponse { results })).into_response();
    rate_limit::add_rate_limit_headers(&mut http_response, &rate_limit_result);
    Ok(http_response)
}

/// Query params for leaderboard.
#[derive(Debug, serde::Deserialize, ToSchema, utoipa::IntoParams)]
pub struct LeaderboardParams {
    /// Filter by content type
    #[schema(example = "post")]
    pub content_type: Option<String>,
    /// Time window: 24h, 7d, 30d, all
    #[schema(example = "7d")]
    pub window: Option<String>,
    /// Max items to return (default 10, max 50)
    #[schema(example = 10)]
    pub limit: Option<i64>,
}

/// GET /v1/likes/top — Top liked content (public)
#[utoipa::path(
    get,
    path = "/v1/likes/top",
    params(LeaderboardParams),
    responses(
        (status = 200, description = "Leaderboard retrieved", body = TopLikedResponse),
        (status = 400, description = "Invalid parameters", body = shared::errors::ApiError),
    ),
    tag = "Leaderboard"
)]
pub async fn get_leaderboard(
    State(state): State<AppState>,
    Query(params): Query<LeaderboardParams>,
) -> Result<impl IntoResponse, ApiErrorResponse> {
    // Validate optional content_type filter
    if let Some(ref ct) = params.content_type {
        validate_content_type(&state, ct)?;
    }

    let window_str = params.window.as_deref().unwrap_or("all");
    let window = TimeWindow::from_str_value(window_str)
        .ok_or_else(|| AppError::InvalidWindow(window_str.to_string()))?;
    let limit = params.limit.unwrap_or(10).clamp(1, 50);
    let response = state
        .like_service()
        .get_leaderboard(params.content_type.as_deref(), window, limit)
        .await?;
    Ok((StatusCode::OK, Json(response)))
}
