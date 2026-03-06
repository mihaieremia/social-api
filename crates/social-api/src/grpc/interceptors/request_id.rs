//! gRPC request ID interceptor.
//!
//! Generates or propagates a unique request identifier via gRPC metadata,
//! mirroring the HTTP `X-Request-Id` middleware behavior.

use tonic::{Request, Status};

use crate::middleware::request_id::fast_request_id;

const REQUEST_ID_KEY: &str = "x-request-id";

/// Extension type inserted into `Request::extensions()` so gRPC service
/// methods can retrieve the request ID without parsing metadata again.
#[derive(Clone, Debug)]
pub struct RequestIdExt(#[allow(dead_code)] pub String);

/// Tonic interceptor that ensures every gRPC request has a unique request ID.
///
/// - If the client sent `x-request-id` in metadata, it is preserved.
/// - Otherwise a fast monotonic ID is generated (same format as the HTTP layer).
///
/// The ID is stored both in metadata (for downstream propagation) and in
/// `Request::extensions()` as `RequestIdExt` for easy handler access.
#[allow(clippy::result_large_err)]
pub fn inject_request_id(mut req: Request<()>) -> Result<Request<()>, Status> {
    let request_id = req
        .metadata()
        .get(REQUEST_ID_KEY)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(fast_request_id);

    if let Ok(val) = request_id.parse() {
        req.metadata_mut().insert(REQUEST_ID_KEY, val);
    }

    req.extensions_mut().insert(RequestIdExt(request_id));
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_request_id_when_missing() {
        let req = Request::new(());
        let result = inject_request_id(req).unwrap();
        let ext = result.extensions().get::<RequestIdExt>().unwrap();
        assert!(
            ext.0.starts_with("req_"),
            "expected req_ prefix, got: {}",
            ext.0
        );
        // Verify it was also set in metadata
        let meta_val = result
            .metadata()
            .get(REQUEST_ID_KEY)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(meta_val, ext.0);
    }

    #[test]
    fn preserves_existing_request_id() {
        let mut req = Request::new(());
        req.metadata_mut()
            .insert(REQUEST_ID_KEY, "custom-id-123".parse().unwrap());

        let result = inject_request_id(req).unwrap();
        let ext = result.extensions().get::<RequestIdExt>().unwrap();
        assert_eq!(ext.0, "custom-id-123");
    }

    #[test]
    fn request_id_ext_is_clone_and_debug() {
        let ext = RequestIdExt("req_abc".to_string());
        let cloned = ext.clone();
        assert_eq!(cloned.0, "req_abc");
        let debug = format!("{ext:?}");
        assert!(debug.contains("req_abc"));
    }
}
