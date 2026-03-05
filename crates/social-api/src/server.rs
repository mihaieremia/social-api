use axum::{
    Router,
    routing::{delete, get, post},
};

use crate::handlers;
use crate::state::AppState;

/// Build the Axum router with all routes and middleware.
pub fn build_router(state: AppState) -> Router {
    let health_routes = Router::new()
        .route("/health/live", get(handlers::health::liveness))
        .route("/health/ready", get(handlers::health::readiness));

    let api_routes = Router::new()
        // Like/Unlike
        .route("/v1/likes", post(handlers::likes::like_content))
        .route(
            "/v1/likes/{content_type}/{content_id}",
            delete(handlers::likes::unlike_content),
        )
        // Count & Status
        .route(
            "/v1/likes/{content_type}/{content_id}/count",
            get(handlers::likes::get_count),
        )
        .route(
            "/v1/likes/{content_type}/{content_id}/status",
            get(handlers::likes::get_status),
        )
        // User likes
        .route("/v1/likes/user", get(handlers::likes::get_user_likes))
        // Batch
        .route("/v1/likes/batch/counts", post(handlers::likes::batch_counts))
        .route(
            "/v1/likes/batch/statuses",
            post(handlers::likes::batch_statuses),
        )
        // Leaderboard
        .route("/v1/likes/top", get(handlers::likes::get_leaderboard));

    Router::new()
        .merge(health_routes)
        .merge(api_routes)
        .with_state(state)
}
