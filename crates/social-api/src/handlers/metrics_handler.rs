use axum::{extract::State, response::IntoResponse};
use metrics_exporter_prometheus::PrometheusHandle;

/// GET /metrics — Prometheus metrics endpoint.
/// Renders all registered metrics in Prometheus exposition format.
pub async fn metrics(State(handle): State<PrometheusHandle>) -> impl IntoResponse {
    let output = handle.render();
    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        output,
    )
}
