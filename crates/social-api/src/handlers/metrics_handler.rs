use axum::{extract::State, response::IntoResponse};
use metrics_exporter_prometheus::PrometheusHandle;

/// GET /metrics — Prometheus metrics endpoint.
/// Renders all registered metrics in Prometheus exposition format.
#[utoipa::path(
    get,
    path = "/metrics",
    responses(
        (status = 200, description = "Prometheus exposition format metrics", content_type = "text/plain; version=0.0.4"),
    ),
    tag = "Health"
)]
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
