use axum::{
    extract::Request,
    http::HeaderValue,
    middleware::Next,
    response::Response,
};
use uuid::Uuid;

const REQUEST_ID_HEADER: &str = "x-request-id";

/// Middleware that generates or propagates X-Request-Id header.
pub async fn inject_request_id(mut request: Request, next: Next) -> Response {
    // Use existing header or generate new UUID
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("req_{}", Uuid::new_v4()));

    // Insert into request extensions for use by handlers
    request.extensions_mut().insert(RequestId(request_id.clone()));

    // Set header on request
    if let Ok(val) = HeaderValue::from_str(&request_id) {
        request.headers_mut().insert(REQUEST_ID_HEADER, val.clone());
    }

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
