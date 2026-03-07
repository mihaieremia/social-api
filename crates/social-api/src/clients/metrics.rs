/// Record metrics for an external service call (counter + histogram).
///
/// `service` and `method` are `&'static str` (always literals).
/// `status` is `String` because HTTP status codes are formatted at runtime.
pub(crate) fn record_external_call(
    service: &'static str,
    method: &'static str,
    status: String,
    latency_secs: f64,
) {
    metrics::counter!(
        "social_api_external_calls_total",
        "service" => service,
        "method" => method,
        "status" => status,
    )
    .increment(1);
    metrics::histogram!(
        "social_api_external_call_duration_seconds",
        "service" => service,
        "method" => method,
    )
    .record(latency_secs);
}
