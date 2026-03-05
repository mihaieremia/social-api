use axum::{
    Router, middleware as axum_middleware,
    routing::{delete, get, post},
};
use metrics_exporter_prometheus::PrometheusHandle;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::handlers;
use crate::middleware;
use crate::openapi::ApiDoc;
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

    // Write routes (POST/DELETE) — per-user write rate limit
    let write_routes = Router::new()
        .route("/v1/likes", post(handlers::likes::like_content))
        .route(
            "/v1/likes/{content_type}/{content_id}",
            delete(handlers::likes::unlike_content),
        )
        .route(
            "/v1/likes/batch/statuses",
            post(handlers::likes::batch_statuses),
        )
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            middleware::rate_limit::write_rate_limit,
        ));

    // Read routes (GET) — per-IP read rate limit
    let read_routes = Router::new()
        .route(
            "/v1/likes/{content_type}/{content_id}/count",
            get(handlers::likes::get_count),
        )
        .route(
            "/v1/likes/{content_type}/{content_id}/status",
            get(handlers::likes::get_status),
        )
        .route("/v1/likes/user", get(handlers::likes::get_user_likes))
        .route(
            "/v1/likes/batch/counts",
            post(handlers::likes::batch_counts),
        )
        .route("/v1/likes/top", get(handlers::likes::get_leaderboard))
        .route("/v1/likes/stream", get(handlers::stream::like_stream))
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            middleware::rate_limit::read_rate_limit,
        ));

    // Merge write + read routes with shared state and global middleware
    let api_routes = Router::new()
        .merge(write_routes)
        .merge(read_routes)
        .with_state(state)
        // Global middleware (outermost-first): request_id → error_context → metrics → rate_limit → handler
        .layer(axum_middleware::from_fn(middleware::metrics::track_metrics))
        .layer(axum_middleware::from_fn(
            middleware::error_context::patch_error_request_id,
        ))
        .layer(axum_middleware::from_fn(
            middleware::request_id::inject_request_id,
        ));

    Router::new()
        .merge(health_routes)
        .merge(metrics_route)
        .merge(api_routes)
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
}
