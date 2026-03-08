#![allow(dead_code)]
//! Shared test infrastructure for in-process axum tests.
//!
//! `TestApp::new()` connects to the shared Postgres and Redis test services,
//! resets them under a cross-process advisory lock, then builds the full Axum
//! router with stub external dependencies.

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use axum::{Router, routing::get};
use http_body_util::BodyExt as _;
use metrics_exporter_prometheus::PrometheusBuilder;
use shared::errors::AppError;
use shared::types::AuthenticatedUser;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;
use uuid::Uuid;

use social_api::clients::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use social_api::clients::content_client::ContentValidator;
use social_api::clients::profile_client::TokenValidator;
use social_api::server::{build_grpc_server, build_router};
use social_api::services::like_service::LikeService;
use social_api::state::AppState;
use tokio::net::TcpListener;

pub mod infra;

/// A fully wired axum router backed by shared Postgres + Redis test services,
/// plus a tonic gRPC server on a random port.
pub struct TestApp {
    pub router: axum::Router,
    pub grpc_addr: std::net::SocketAddr,
    _grpc_handle: tokio::task::JoinHandle<()>,
    _content_health_handle: tokio::task::JoinHandle<()>,
    _context: infra::TestContext,
}

impl TestApp {
    pub async fn new() -> Self {
        let mut context = infra::TestContext::new().await;

        let content_health_router = Router::new().route("/health", get(|| async { "ok" }));
        let content_health_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("content health listener");
        let content_health_addr = content_health_listener
            .local_addr()
            .expect("content health local addr");
        let content_health_url = format!("http://{content_health_addr}");
        for url in context.config.content_api_urls.values_mut() {
            url.clone_from(&content_health_url);
        }

        let content_breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 5,
            recovery_timeout: std::time::Duration::from_secs(30),
            success_threshold: 2,
            service_name: "content_api_test".to_string(),
            rate_window: std::time::Duration::from_secs(30),
            failure_rate_threshold: 0.5,
            min_calls_for_rate: 10,
        }));

        let like_service = LikeService::new_with_validator(
            context.db.clone(),
            context.cache.clone(),
            Arc::new(TestContentValidator),
            context.config.clone(),
            content_breaker,
        );

        let shutdown_token = CancellationToken::new();

        let state = AppState::new_for_test(
            context.db.clone(),
            context.cache.clone(),
            context.config.clone(),
            shutdown_token,
            Box::new(TestTokenValidator),
            like_service,
        );

        let metrics_handle = PrometheusBuilder::new().build_recorder().handle();

        let router = build_router(state.clone(), metrics_handle);

        // Boot gRPC on a random available port
        let grpc_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("grpc listener");
        let grpc_addr = grpc_listener.local_addr().expect("grpc local addr");

        let grpc_server = build_grpc_server(state);
        let _grpc_handle = tokio::spawn(async move {
            grpc_server
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(
                    grpc_listener,
                ))
                .await
                .expect("grpc serve");
        });

        let _content_health_handle = tokio::spawn(async move {
            axum::serve(content_health_listener, content_health_router)
                .await
                .expect("content health serve");
        });

        // Give the server a moment to bind
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        TestApp {
            router,
            grpc_addr,
            _context: context,
            _grpc_handle,
            _content_health_handle,
        }
    }

    /// Send a single request through the router using tower's `oneshot`.
    #[allow(dead_code)]
    pub async fn request(&self, req: Request<Body>) -> axum::response::Response {
        self.router
            .clone()
            .oneshot(req)
            .await
            .expect("router oneshot failed")
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        self._grpc_handle.abort();
        self._content_health_handle.abort();
    }
}

/// Stub token validator: the Bearer token must be a plain UUID.
/// The UUID becomes the authenticated user_id directly.
pub struct TestTokenValidator;

#[async_trait::async_trait]
impl TokenValidator for TestTokenValidator {
    async fn validate(&self, token: &str) -> Result<AuthenticatedUser, AppError> {
        let user_id = Uuid::parse_str(token)
            .map_err(|_| AppError::Unauthorized("token must be a UUID in tests".into()))?;
        Ok(AuthenticatedUser {
            user_id,
            display_name: "Test User".to_string(),
        })
    }
}

/// Stub content validator: all content is considered valid.
pub struct TestContentValidator;

#[async_trait::async_trait]
impl ContentValidator for TestContentValidator {
    async fn validate(&self, _content_type: &str, _content_id: Uuid) -> Result<bool, AppError> {
        Ok(true)
    }
}

// ── Request builder helpers ──────────────────────────────────────────────────

#[allow(dead_code)]
pub fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[allow(dead_code)]
pub fn authed_json_request(
    method: &str,
    uri: &str,
    user_id: Uuid,
    body: serde_json::Value,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {user_id}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[allow(dead_code)]
pub fn get_request(uri: &str, user_id: Option<Uuid>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(id) = user_id {
        builder = builder.header("authorization", format!("Bearer {id}"));
    }
    builder.body(Body::empty()).unwrap()
}

/// Collect the full response body and deserialize it as JSON.
#[allow(dead_code)]
pub async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}
