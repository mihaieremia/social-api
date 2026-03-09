//! gRPC metrics recording helpers.
//!
//! Records request counts and latency histograms for gRPC methods,
//! mirroring the HTTP metrics middleware.

use std::time::Instant;
use tonic::Code;

/// Record a completed gRPC request in Prometheus metrics.
///
/// - `method`: the fully-qualified gRPC method name (e.g., `"social.v1.LikeService/Like"`).
/// - `code`: the gRPC status code string (e.g., `"OK"`, `"NOT_FOUND"`).
/// - `start`: the `Instant` captured at the beginning of the handler.
pub fn record_grpc_request(method: &'static str, code: &'static str, start: Instant) {
    let latency = start.elapsed().as_secs_f64();
    metrics::counter!(
        "social_api_grpc_requests_total",
        "method" => method,
        "code" => code,
    )
    .increment(1);
    metrics::histogram!(
        "social_api_grpc_request_duration_seconds",
        "method" => method,
    )
    .record(latency);
}

/// Convert tonic status codes to stable Prometheus label values.
pub fn grpc_code_label(code: Code) -> &'static str {
    match code {
        Code::Ok => "OK",
        Code::Cancelled => "CANCELLED",
        Code::Unknown => "UNKNOWN",
        Code::InvalidArgument => "INVALID_ARGUMENT",
        Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
        Code::NotFound => "NOT_FOUND",
        Code::AlreadyExists => "ALREADY_EXISTS",
        Code::PermissionDenied => "PERMISSION_DENIED",
        Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
        Code::FailedPrecondition => "FAILED_PRECONDITION",
        Code::Aborted => "ABORTED",
        Code::OutOfRange => "OUT_OF_RANGE",
        Code::Unimplemented => "UNIMPLEMENTED",
        Code::Internal => "INTERNAL",
        Code::Unavailable => "UNAVAILABLE",
        Code::DataLoss => "DATA_LOSS",
        Code::Unauthenticated => "UNAUTHENTICATED",
    }
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

    #[test]
    fn grpc_code_label_matches_expected_prometheus_values() {
        assert_eq!(grpc_code_label(Code::Ok), "OK");
        assert_eq!(
            grpc_code_label(Code::ResourceExhausted),
            "RESOURCE_EXHAUSTED"
        );
        assert_eq!(grpc_code_label(Code::Unavailable), "UNAVAILABLE");
    }

    #[test]
    fn grpc_code_label_covers_all_tonic_codes() {
        let cases = [
            (Code::Ok, "OK"),
            (Code::Cancelled, "CANCELLED"),
            (Code::Unknown, "UNKNOWN"),
            (Code::InvalidArgument, "INVALID_ARGUMENT"),
            (Code::DeadlineExceeded, "DEADLINE_EXCEEDED"),
            (Code::NotFound, "NOT_FOUND"),
            (Code::AlreadyExists, "ALREADY_EXISTS"),
            (Code::PermissionDenied, "PERMISSION_DENIED"),
            (Code::ResourceExhausted, "RESOURCE_EXHAUSTED"),
            (Code::FailedPrecondition, "FAILED_PRECONDITION"),
            (Code::Aborted, "ABORTED"),
            (Code::OutOfRange, "OUT_OF_RANGE"),
            (Code::Unimplemented, "UNIMPLEMENTED"),
            (Code::Internal, "INTERNAL"),
            (Code::Unavailable, "UNAVAILABLE"),
            (Code::DataLoss, "DATA_LOSS"),
            (Code::Unauthenticated, "UNAUTHENTICATED"),
        ];
        for (code, expected) in cases {
            assert_eq!(grpc_code_label(code), expected, "failed for {code:?}");
        }
    }
}
