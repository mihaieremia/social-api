//! SSE stream integration tests for the Social API.
//!
//! Tests 1 & 2 use the in-process tower `oneshot` approach (same as http_test.rs).
//! Tests 3 & 4 bind the router to a real TCP listener and use `reqwest` for streaming
//! because SSE responses are inherently long-lived and cannot be collected via oneshot.
//!
//! Run: cargo test --test stream_test -- --test-threads=1

mod common;
use common::{TestApp, body_json, get_request};

use axum::http::StatusCode;
use axum::{Router, routing::get};
use metrics_exporter_prometheus::PrometheusBuilder;
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use social_api::clients::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use social_api::server::build_router;
use social_api::services::like_service::LikeService;
use social_api::state::AppState;

// ============================================================
// Test 1: Unknown content type returns 400
// ============================================================

#[tokio::test]
async fn test_stream_unknown_content_type_returns_400() {
    let app = TestApp::new().await;
    let content_id = Uuid::new_v4();
    let req = get_request(
        &format!("/v1/likes/stream?content_type=unknown_type&content_id={content_id}"),
        None,
    );
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(
        body["error"]["code"], "CONTENT_TYPE_UNKNOWN",
        "expected CONTENT_TYPE_UNKNOWN error code, got: {body}"
    );
}

// ============================================================
// Test 2: Invalid UUID returns 400
// ============================================================

#[tokio::test]
async fn test_stream_invalid_uuid_returns_400() {
    let app = TestApp::new().await;
    let req = get_request(
        "/v1/likes/stream?content_type=post&content_id=not-a-uuid",
        None,
    );
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ============================================================
// Helpers for networked SSE tests (tests 3 & 4)
// ============================================================

/// Holds all resources for a networked test server (real TCP socket).
struct NetworkedTestApp {
    /// Base URL for the server (e.g. "http://127.0.0.1:PORT")
    pub base_url: String,
    /// AppState so tests can call cache().publish() etc.
    pub state: AppState,
    _server_handle: tokio::task::JoinHandle<()>,
    _content_health_handle: tokio::task::JoinHandle<()>,
    /// Keep the test scope alive (holds advisory lock + containers)
    _context: common::infra::TestContext,
}

impl NetworkedTestApp {
    async fn new(shutdown_token: CancellationToken) -> Self {
        let mut context = common::infra::TestContext::new().await;

        // Spin up a minimal content health server so the circuit breaker doesn't trip.
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

        // Override the heartbeat interval to something very short so tests
        // don't need to wait 10 seconds for the first heartbeat.
        context.config.sse_heartbeat_interval_secs = 1;

        let content_breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 5,
            recovery_timeout: Duration::from_secs(30),
            success_threshold: 2,
            service_name: "content_api_test".to_string(),
            rate_window: Duration::from_secs(30),
            failure_rate_threshold: 0.5,
            min_calls_for_rate: 10,
        }));

        let like_service = LikeService::new_with_validator(
            context.db.clone(),
            context.cache.clone(),
            Arc::new(common::TestContentValidator),
            context.config.clone(),
            content_breaker,
        );

        let state = AppState::new_for_test(
            context.db.clone(),
            context.cache.clone(),
            context.config.clone(),
            shutdown_token,
            Box::new(common::TestTokenValidator),
            like_service,
        );

        let metrics_handle = PrometheusBuilder::new().build_recorder().handle();
        let router = build_router(state.clone(), metrics_handle);

        // Bind to a random available port.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind server listener");
        let addr = listener.local_addr().expect("server local addr");
        let base_url = format!("http://{addr}");

        let _server_handle = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("server serve");
        });

        let _content_health_handle = tokio::spawn(async move {
            axum::serve(content_health_listener, content_health_router)
                .await
                .expect("content health serve");
        });

        // Give the server a moment to start accepting connections.
        tokio::time::sleep(Duration::from_millis(50)).await;

        NetworkedTestApp {
            base_url,
            state,
            _server_handle,
            _content_health_handle,
            _context: context,
        }
    }
}

impl Drop for NetworkedTestApp {
    fn drop(&mut self) {
        self._server_handle.abort();
        self._content_health_handle.abort();
    }
}

// ============================================================
// Test 3: SSE stream receives a published event
// ============================================================

#[tokio::test]
async fn test_stream_receives_published_event() {
    let shutdown_token = CancellationToken::new();
    let app = NetworkedTestApp::new(shutdown_token.clone()).await;

    let content_id = Uuid::new_v4();
    let url = format!(
        "{}/v1/likes/stream?content_type=post&content_id={content_id}",
        app.base_url
    );

    // Connect to SSE endpoint.
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("build reqwest client");

    let mut response = client
        .get(&url)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .expect("SSE connect failed");

    assert_eq!(response.status(), 200, "SSE endpoint should return 200 OK");

    // Wait for the SSE connection to be fully established before publishing.
    // We wait for the first heartbeat (arrives within ~1s per our 1s heartbeat interval).
    let heartbeat_received = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match response.chunk().await {
                Ok(Some(bytes)) => {
                    let text = String::from_utf8_lossy(&bytes);
                    if text.contains("heartbeat") {
                        return true;
                    }
                }
                Ok(None) => return false,
                Err(_) => return false,
            }
        }
    })
    .await;

    assert!(
        matches!(heartbeat_received, Ok(true)),
        "Expected to receive a heartbeat within 5s confirming SSE connection is active"
    );

    // Give the PubSubManager bridge task time to establish the Redis SUBSCRIBE
    // connection. The bridge is spawned in the background when subscribe() is
    // called, so a small wait ensures the Redis subscription is active before
    // we publish.
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Publish a like event to the channel the SSE stream is subscribed to.
    let channel = format!("sse:post:{content_id}");
    let like_event = serde_json::json!({
        "event": "like",
        "user_id": Uuid::new_v4(),
        "count": 1,
        "timestamp": chrono::Utc::now().to_rfc3339()
    });
    let event_json = serde_json::to_string(&like_event).expect("serialize event");

    app.state.cache().publish(&channel, &event_json).await;

    // Read the next SSE data chunk — it should contain our event.
    let event_received = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match response.chunk().await {
                Ok(Some(bytes)) => {
                    let text = String::from_utf8_lossy(&bytes);
                    if let Some(json_str) = text.lines().find(|l| l.starts_with("data:")) {
                        let json_str = json_str.trim_start_matches("data:").trim();
                        let parsed: serde_json::Value = serde_json::from_str(json_str).unwrap();
                        assert_eq!(
                            parsed["event"], "like",
                            "Expected like event, got: {}",
                            parsed
                        );
                        return true;
                    }
                }
                Ok(None) => return false,
                Err(_) => return false,
            }
        }
    })
    .await;

    assert!(
        matches!(event_received, Ok(true)),
        "Expected to receive a 'like' event SSE data chunk after publishing to Redis channel"
    );

    // Cleanup.
    shutdown_token.cancel();
}

// ============================================================
// Test 4: SSE stream closes when shutdown token is cancelled
// ============================================================

#[tokio::test]
async fn test_stream_closes_on_shutdown() {
    let shutdown_token = CancellationToken::new();
    let app = NetworkedTestApp::new(shutdown_token.clone()).await;

    let content_id = Uuid::new_v4();
    let url = format!(
        "{}/v1/likes/stream?content_type=post&content_id={content_id}",
        app.base_url
    );

    // Connect to SSE endpoint.
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("build reqwest client");

    let mut response = client
        .get(&url)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .expect("SSE connect failed");

    assert_eq!(response.status(), 200, "SSE endpoint should return 200 OK");

    // Wait for the first heartbeat to confirm the stream is active.
    let heartbeat_received = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match response.chunk().await {
                Ok(Some(bytes)) => {
                    let text = String::from_utf8_lossy(&bytes);
                    if text.contains("heartbeat") {
                        return true;
                    }
                }
                Ok(None) => return false,
                Err(_) => return false,
            }
        }
    })
    .await;

    assert!(
        matches!(heartbeat_received, Ok(true)),
        "Expected a heartbeat within 5s confirming SSE connection is active"
    );

    // Cancel the shutdown token — the stream should emit a shutdown event and close.
    shutdown_token.cancel();

    // Verify the stream terminates: we either receive a shutdown event or the stream closes.
    let stream_ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match response.chunk().await {
                Ok(Some(bytes)) => {
                    let text = String::from_utf8_lossy(&bytes);
                    if text.contains("shutdown") {
                        return true;
                    }
                }
                // Stream closed (None) without an explicit shutdown event is also acceptable.
                Ok(None) => return true,
                Err(_) => return true,
            }
        }
    })
    .await;

    assert!(
        matches!(stream_ended, Ok(true)),
        "Expected the SSE stream to end (shutdown event or stream close) after shutdown token cancellation"
    );
}
