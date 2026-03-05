use axum::{extract::Request, middleware::Next, response::Response};
use std::time::Instant;

/// Middleware that records HTTP request metrics.
pub async fn track_metrics(request: Request, next: Next) -> Response {
    let method = request.method().as_str().to_owned();
    let path = normalize_path(request.uri().path());
    let start = Instant::now();

    let response = next.run(request).await;

    let status = response.status().as_u16().to_string();
    let latency = start.elapsed().as_secs_f64();

    metrics::counter!(
        "social_api_http_requests_total",
        "method" => method.clone(),
        "path" => path.clone(),
        "status" => status,
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
fn normalize_path(path: &str) -> String {
    let mut result = String::with_capacity(path.len());
    for (i, part) in path.split('/').enumerate() {
        if i > 0 {
            result.push('/');
        }
        if uuid::Uuid::parse_str(part).is_ok() || part.parse::<i64>().is_ok() {
            result.push_str(":id");
        } else {
            result.push_str(part);
        }
    }
    result
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

    handle
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
