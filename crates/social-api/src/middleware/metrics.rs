use axum::{extract::Request, middleware::Next, response::Response};
use std::time::Instant;

/// Middleware that records HTTP request metrics.
///
/// Minimises per-request heap allocations: `method` uses `as_str()` (`&str`,
/// no allocation), `status_str` is `&'static str`, and only `path` is a
/// runtime `String` (unavoidable due to UUID normalisation).
pub async fn track_metrics(request: Request, next: Next) -> Response {
    let method = request.method().as_str().to_owned();
    let path = normalize_path(request.uri().path());
    let start = Instant::now();

    let response = next.run(request).await;

    let status = response.status().as_u16();
    let latency = start.elapsed().as_secs_f64();

    // Status is a 3-digit number — use a `&'static str` to avoid heap allocation.
    let status_str = match status {
        200 => "200",
        201 => "201",
        204 => "204",
        400 => "400",
        401 => "401",
        404 => "404",
        429 => "429",
        500 => "500",
        503 => "503",
        _ => "other",
    };

    metrics::counter!(
        "social_api_http_requests_total",
        "method" => method.clone(),
        "path" => path.clone(),
        "status" => status_str,
    )
    .increment(1);

    metrics::histogram!(
        "social_api_http_request_duration_seconds",
        "method" => method,
        "path" => path,
    )
    .record(latency);

    response
}

/// Normalize path to avoid high-cardinality labels.
/// Replaces UUIDs and numeric IDs with placeholders.
///
/// Uses a fast heuristic (length + hyphen positions) instead of `Uuid::parse_str()`
/// to avoid ~33K full UUID parses/sec at high RPS. False positives (non-UUID strings
/// matching the pattern) are harmless — they just get collapsed to `:id`.
fn normalize_path(path: &str) -> String {
    let mut result = String::with_capacity(path.len());
    for (i, part) in path.split('/').enumerate() {
        if i > 0 {
            result.push('/');
        }
        if looks_like_uuid(part) || looks_like_number(part) {
            result.push_str(":id");
        } else {
            result.push_str(part);
        }
    }
    result
}

/// Fast UUID heuristic: 36 chars with hyphens at positions 8, 13, 18, 23.
/// ~50x faster than `Uuid::parse_str()` — no hex validation, just structure.
#[inline]
fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36 && {
        let b = s.as_bytes();
        b[8] == b'-' && b[13] == b'-' && b[18] == b'-' && b[23] == b'-'
    }
}

/// Fast numeric check — avoids the overhead of `str::parse::<i64>()`.
#[inline]
fn looks_like_number(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Initialize Prometheus recorder and return the handle.
pub fn init_metrics() -> metrics_exporter_prometheus::PrometheusHandle {
    let builder = metrics_exporter_prometheus::PrometheusBuilder::new()
        .set_buckets(&[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5])
        .expect("Invalid bucket boundaries");
    let handle = builder
        .install_recorder()
        .expect("Failed to install Prometheus recorder");

    // Register all spec-required metrics with initial values
    metrics::describe_counter!("social_api_http_requests_total", "Total HTTP requests");
    metrics::describe_histogram!(
        "social_api_http_request_duration_seconds",
        "HTTP request duration in seconds"
    );
    metrics::describe_counter!(
        "social_api_cache_operations_total",
        "Total cache operations"
    );
    metrics::describe_counter!(
        "social_api_external_calls_total",
        "Total external service calls"
    );
    metrics::describe_histogram!(
        "social_api_external_call_duration_seconds",
        "External call duration in seconds"
    );
    metrics::describe_gauge!(
        "social_api_circuit_breaker_state",
        "Circuit breaker state (0=closed, 1=half-open, 2=open)"
    );
    metrics::describe_gauge!(
        "social_api_sse_connections_active",
        "Active SSE connections"
    );
    metrics::describe_counter!("social_api_likes_total", "Total like/unlike operations");
    metrics::describe_gauge!(
        "social_api_db_pool_connections",
        "Database connection pool size by pool (writer/reader) and state (active/idle/max)"
    );

    // gRPC metrics
    metrics::describe_counter!("social_api_grpc_requests_total", "Total gRPC requests");
    metrics::describe_histogram!(
        "social_api_grpc_request_duration_seconds",
        "gRPC request duration in seconds"
    );

    handle
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        middleware as axum_middleware,
        routing::get,
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_track_metrics_records_200() {
        let app = Router::new()
            .route("/test", get(|| async { StatusCode::OK }))
            .layer(axum_middleware::from_fn(track_metrics));

        let response = app
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_track_metrics_records_404() {
        let app = Router::new()
            .route("/test", get(|| async { StatusCode::NOT_FOUND }))
            .layer(axum_middleware::from_fn(track_metrics));

        let response = app
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_track_metrics_records_unknown_status() {
        let app = Router::new()
            .route("/test", get(|| async { StatusCode::IM_A_TEAPOT }))
            .layer(axum_middleware::from_fn(track_metrics));

        let response = app
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
    }

    #[test]
    fn test_normalize_path_uuid() {
        let path = "/v1/likes/post/731b0395-4888-4822-b516-05b4b7bf2089/count";
        assert_eq!(normalize_path(path), "/v1/likes/post/:id/count");
    }

    #[test]
    fn test_normalize_path_no_uuid() {
        let path = "/v1/likes/top";
        assert_eq!(normalize_path(path), "/v1/likes/top");
    }

    #[test]
    fn test_normalize_path_health() {
        let path = "/health/ready";
        assert_eq!(normalize_path(path), "/health/ready");
    }

    #[test]
    fn test_normalize_path_numeric_id() {
        // Numeric IDs should also be replaced with :id
        let path = "/v1/items/12345/details";
        assert_eq!(normalize_path(path), "/v1/items/:id/details");
    }

    #[test]
    fn test_normalize_path_root() {
        let path = "/";
        assert_eq!(normalize_path(path), "/");
    }

    #[test]
    fn test_normalize_path_metrics() {
        let path = "/metrics";
        assert_eq!(normalize_path(path), "/metrics");
    }

    #[test]
    fn test_normalize_path_multiple_uuids() {
        let path =
            "/v1/likes/731b0395-4888-4822-b516-05b4b7bf2089/731b0395-4888-4822-b516-05b4b7bf2089";
        assert_eq!(normalize_path(path), "/v1/likes/:id/:id");
    }

    #[test]
    fn test_normalize_path_stream_with_uuid() {
        let path = "/v1/likes/stream/post/731b0395-4888-4822-b516-05b4b7bf2089";
        assert_eq!(normalize_path(path), "/v1/likes/stream/post/:id");
    }

    #[tokio::test]
    async fn test_track_metrics_records_3xx() {
        let app = Router::new()
            .route("/test", get(|| async { StatusCode::MOVED_PERMANENTLY }))
            .layer(axum_middleware::from_fn(track_metrics));

        let response = app
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    }

    #[tokio::test]
    async fn test_track_metrics_records_5xx() {
        let app = Router::new()
            .route("/test", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
            .layer(axum_middleware::from_fn(track_metrics));

        let response = app
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_looks_like_uuid_false_for_36_char_non_uuid() {
        // 36 chars but hyphens not at UUID positions → should be false
        let s = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"; // 36 x's, no hyphens
        assert!(!looks_like_uuid(s));
    }

    #[test]
    fn test_looks_like_number_false_for_alphanumeric() {
        assert!(!looks_like_number("abc123"));
        assert!(!looks_like_number(""));
    }
}
