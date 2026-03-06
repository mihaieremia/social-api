use axum::{
    Router, middleware as axum_middleware,
    routing::{delete, get, post},
};
use metrics_exporter_prometheus::PrometheusHandle;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::handlers;
use crate::middleware;
use crate::openapi::ApiDoc;
use crate::state::AppState;

/// Custom `MakeSpan` that pre-registers `request_id` and `user_id` as empty fields.
///
/// - `request_id` is recorded by `inject_request_id` middleware (runs inside this span).
/// - `user_id` is recorded by `AuthUser` extractor on authenticated requests.
#[derive(Clone)]
struct MakeRequestSpan;

impl<B> tower_http::trace::MakeSpan<B> for MakeRequestSpan {
    fn make_span(&mut self, request: &axum::http::Request<B>) -> tracing::Span {
        tracing::info_span!(
            "request",
            method = %request.method(),
            path = %request.uri().path(),
            request_id = tracing::field::Empty,
            user_id = tracing::field::Empty,
        )
    }
}

/// Build the Axum router with all routes and middleware.
pub fn build_router(state: AppState, metrics_handle: PrometheusHandle) -> Router {
    let concurrency_limit = state.config().server_concurrency_limit;
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
        .route(
            "/v1/likes/batch/statuses",
            post(handlers::likes::batch_statuses),
        )
        .route("/v1/likes/top", get(handlers::likes::get_leaderboard))
        .route("/v1/likes/stream", get(handlers::stream::like_stream))
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            middleware::rate_limit::read_rate_limit,
        ));

    // Merge write + read routes with shared state and global middleware
    // Layer order (last added = outermost = runs first):
    //   TraceLayer → ConcurrencyLimit → inflight → request_id → error_context → metrics → rate_limit → handler
    //
    // ConcurrencyLimitLayer sheds excess load with 503 before allocating resources.
    // TraceLayer is outermost so its span is active for all subsequent middleware.
    // inject_request_id records request_id into the span; AuthUser extractor records user_id.
    let api_routes = Router::new()
        .merge(write_routes)
        .merge(read_routes)
        .with_state(state.clone())
        .layer(axum_middleware::from_fn(middleware::metrics::track_metrics))
        .layer(axum_middleware::from_fn(
            middleware::error_context::patch_error_request_id,
        ))
        .layer(axum_middleware::from_fn(
            middleware::request_id::inject_request_id,
        ))
        .layer(axum_middleware::from_fn_with_state(
            state,
            middleware::inflight::track_inflight,
        ))
        .layer(ConcurrencyLimitLayer::new(concurrency_limit))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(MakeRequestSpan)
                .on_response(
                    tower_http::trace::DefaultOnResponse::new()
                        .level(tracing::Level::INFO)
                        .latency_unit(tower_http::LatencyUnit::Millis),
                ),
        );

    Router::new()
        .merge(health_routes)
        .merge(metrics_route)
        .merge(api_routes)
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
}
