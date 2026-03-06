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

/// Access log verbosity level (set via `ACCESS_LOG` env var).
///
/// - `full`: log every request at INFO (development / low-traffic).
/// - `errors`: log only 4xx/5xx or requests slower than 500ms (production default).
/// - `none`: suppress all per-request logs; rely on Prometheus metrics only.
#[derive(Clone, Copy, PartialEq)]
pub enum AccessLogLevel {
    Full,
    Errors,
    None,
}

impl AccessLogLevel {
    pub fn from_env() -> Self {
        match std::env::var("ACCESS_LOG")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "full" | "all" => Self::Full,
            "none" | "off" | "false" | "0" => Self::None,
            // Default: only log noteworthy requests
            _ => Self::Errors,
        }
    }
}

/// Custom `MakeSpan` that creates spans at the appropriate level based on access log config.
///
/// At `errors`/`none` mode, uses `debug_span!` so span creation is nearly free when
/// RUST_LOG filters out debug. At `full` mode, uses `info_span!` for complete logging.
///
/// Fields `request_id` and `user_id` are recorded by downstream middleware/extractors.
#[derive(Clone)]
struct MakeRequestSpan {
    level: AccessLogLevel,
}

impl<B> tower_http::trace::MakeSpan<B> for MakeRequestSpan {
    fn make_span(&mut self, request: &axum::http::Request<B>) -> tracing::Span {
        match self.level {
            AccessLogLevel::Full => tracing::info_span!(
                "request",
                method = %request.method(),
                path = %request.uri().path(),
                request_id = tracing::field::Empty,
                user_id = tracing::field::Empty,
            ),
            _ => tracing::debug_span!(
                "request",
                method = %request.method(),
                path = %request.uri().path(),
                request_id = tracing::field::Empty,
                user_id = tracing::field::Empty,
            ),
        }
    }
}

/// Custom `OnResponse` that conditionally logs based on access log level.
///
/// - `Full`: log every response at INFO with latency.
/// - `Errors`: log only 4xx/5xx status codes or slow requests (>500ms) at WARN.
/// - `None`: no per-request logging at all (metrics handle observability).
#[derive(Clone)]
struct ConditionalOnResponse {
    level: AccessLogLevel,
}

impl<B> tower_http::trace::OnResponse<B> for ConditionalOnResponse {
    fn on_response(
        self,
        response: &axum::http::Response<B>,
        latency: std::time::Duration,
        _span: &tracing::Span,
    ) {
        let status = response.status().as_u16();
        let latency_ms = latency.as_secs_f64() * 1000.0;

        match self.level {
            AccessLogLevel::Full => {
                tracing::info!(status, latency_ms, "response");
            }
            AccessLogLevel::Errors => {
                if status >= 400 || latency_ms > 500.0 {
                    tracing::warn!(status, latency_ms, "slow or error response");
                }
            }
            AccessLogLevel::None => {}
        }
    }
}

/// Build the Axum router with all routes and middleware.
pub fn build_router(state: AppState, metrics_handle: PrometheusHandle) -> Router {
    let concurrency_limit = state.config().server_concurrency_limit;
    let access_log = AccessLogLevel::from_env();
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
                .make_span_with(MakeRequestSpan { level: access_log })
                .on_response(ConditionalOnResponse { level: access_log }),
        );

    Router::new()
        .merge(health_routes)
        .merge(metrics_route)
        .merge(api_routes)
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
}
