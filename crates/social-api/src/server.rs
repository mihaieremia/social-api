use axum::{
    Router,
    middleware as axum_middleware,
    routing::{delete, get, post},
};
use metrics_exporter_prometheus::PrometheusHandle;

use crate::handlers;
use crate::middleware;
use crate::state::AppState;

/// Build the Axum router with all routes and middleware.
pub fn build_router(state: AppState, metrics_handle: PrometheusHandle) -> Router {
    let health_routes = Router::new()
        .route("/health/live", get(handlers::health::liveness))
        .route("/health/ready", get(handlers::health::readiness))
        .with_state(state.clone());

    // Metrics endpoint uses PrometheusHandle as state
    let metrics_route = Router::new()
        .route("/metrics", get(handlers::metrics_handler::metrics))
        .with_state(metrics_handle);

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
        .route("/v1/likes/top", get(handlers::likes::get_leaderboard))
        // SSE stream
        .route("/v1/likes/stream", get(handlers::stream::like_stream))
        .with_state(state)
        // Middleware (applied to API routes)
        .layer(axum_middleware::from_fn(middleware::metrics::track_metrics))
        .layer(axum_middleware::from_fn(middleware::request_id::inject_request_id));

    Router::new()
        .merge(health_routes)
        .merge(metrics_route)
        .merge(api_routes)
}
