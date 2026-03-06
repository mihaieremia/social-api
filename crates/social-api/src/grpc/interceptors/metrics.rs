//! gRPC metrics recording helpers.
//!
//! Records request counts and latency histograms for gRPC methods,
//! mirroring the HTTP metrics middleware.

// Building block for gRPC service implementations (Task 8+).
#![allow(dead_code)]

use std::time::Instant;

/// Record a completed gRPC request in Prometheus metrics.
///
/// - `method`: the fully-qualified gRPC method name (e.g., `"social.v1.LikeService/Like"`).
/// - `code`: the gRPC status code string (e.g., `"OK"`, `"NOT_FOUND"`).
/// - `start`: the `Instant` captured at the beginning of the handler.
pub fn record_grpc_request(method: &str, code: &str, start: Instant) {
    let latency = start.elapsed().as_secs_f64();
    metrics::counter!(
        "social_api_grpc_requests_total",
        "method" => method.to_owned(),
        "code" => code.to_owned(),
    )
    .increment(1);
    metrics::histogram!(
        "social_api_grpc_request_duration_seconds",
        "method" => method.to_owned(),
    )
    .record(latency);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_grpc_request_does_not_panic() {
        // Metrics macros register lazily; just verify no panic on first call.
        let start = Instant::now();
        record_grpc_request("social.v1.LikeService/Like", "OK", start);
    }

    #[test]
    fn record_grpc_request_with_error_code() {
        let start = Instant::now();
        record_grpc_request("social.v1.LikeService/Like", "UNAUTHENTICATED", start);
    }
}
