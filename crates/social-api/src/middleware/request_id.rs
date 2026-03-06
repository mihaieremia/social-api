use axum::{extract::Request, http::HeaderValue, middleware::Next, response::Response};
use std::sync::atomic::{AtomicU64, Ordering};

const REQUEST_ID_HEADER: &str = "x-request-id";

/// Monotonic counter for request ID generation.
/// Much cheaper than `Uuid::new_v4()` which calls `getrandom()` (OS entropy syscall)
/// on every invocation — ~11K syscalls/sec at high RPS.
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a fast, unique-enough request ID without entropy syscalls.
/// Format: `req_{hex_timestamp}_{hex_counter}` — sortable and collision-free
/// within a single process. For cross-replica uniqueness, add a replica ID prefix.
pub fn fast_request_id() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let seq = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("req_{ts:x}-{seq:x}")
}

/// Middleware that generates or propagates X-Request-Id header.
pub async fn inject_request_id(mut request: Request, next: Next) -> Response {
    // Use existing header or generate new fast ID
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(fast_request_id);

    // Insert into request extensions for use by handlers
    request
        .extensions_mut()
        .insert(RequestId(request_id.clone()));

    // Set header on request
    if let Ok(val) = HeaderValue::from_str(&request_id) {
        request.headers_mut().insert(REQUEST_ID_HEADER, val.clone());
    }

    // Record into the active TraceLayer span (created by the outermost layer).
    tracing::Span::current().record("request_id", &request_id);

    let mut response = next.run(request).await;

    // Add to response headers
    if let Ok(val) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, val);
    }

    response
}

/// Request ID stored in request extensions.
#[derive(Clone, Debug)]
pub struct RequestId(pub String);
