/// Record metrics for an external service call (counter + histogram).
pub(crate) fn record_external_call(service: &str, method: &str, status: &str, latency_secs: f64) {
    metrics::counter!(
        "social_api_external_calls_total",
        "service" => service.to_string(),
        "method" => method.to_string(),
        "status" => status.to_string(),
    )
    .increment(1);
    metrics::histogram!(
        "social_api_external_call_duration_seconds",
        "service" => service.to_string(),
        "method" => method.to_string(),
    )
    .record(latency_secs);
}
