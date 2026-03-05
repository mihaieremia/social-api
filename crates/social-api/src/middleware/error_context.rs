use axum::{body::Body, extract::Request, http::HeaderValue, middleware::Next, response::Response};

use crate::middleware::request_id::RequestId;

/// Middleware that patches `request_id` into error JSON responses.
///
/// Runs after the handler. For non-2xx responses with JSON bodies,
/// deserializes the body, sets `error.request_id` from the RequestId
/// extension, and re-serializes. This is the single source of truth
/// for request_id in error responses — handlers never think about it.
pub async fn patch_error_request_id(request: Request, next: Next) -> Response {
    // Extract request_id before passing request to next
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|r| r.0.clone())
        .unwrap_or_else(|| "unknown".to_string());

    let response = next.run(request).await;

    // Only patch non-2xx JSON responses
    if response.status().is_success() {
        return response;
    }

    let is_json = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("application/json"));

    if !is_json {
        return response;
    }

    // Decompose response, read body, patch request_id
    let (mut parts, body) = response.into_parts();

    let bytes = match axum::body::to_bytes(body, 1024 * 64).await {
        Ok(b) => b,
        Err(_) => return Response::from_parts(parts, Body::empty()),
    };

    let patched = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(mut json) => {
            if let Some(error_obj) = json.get_mut("error")
                && let Some(obj) = error_obj.as_object_mut()
            {
                obj.insert(
                    "request_id".to_string(),
                    serde_json::Value::String(request_id),
                );
            }
            let new_bytes = serde_json::to_vec(&json).unwrap_or_else(|_| bytes.to_vec());
            // Update content-length
            if let Ok(len) = HeaderValue::from_str(&new_bytes.len().to_string()) {
                parts.headers.insert("content-length", len);
            }
            new_bytes
        }
        Err(_) => bytes.to_vec(),
    };

    Response::from_parts(parts, Body::from(patched))
}
