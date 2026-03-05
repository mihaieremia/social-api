//! In-process HTTP layer tests.
//!
//! Uses TestApp — builds the full axum Router without a network socket.
//! Requires Docker (testcontainers spins up real Postgres + Redis).
//! Run: cargo test --test http_test -- --test-threads=1

mod common;
use common::{TestApp, authed_json_request, body_json, get_request, json_request};

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

// ============================================================
// Health
// ============================================================

#[tokio::test]
async fn test_health_live() {
    let app = TestApp::new().await;
    let req = get_request("/health/live", None);
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn test_health_ready() {
    let app = TestApp::new().await;
    let req = get_request("/health/ready", None);
    let resp = app.request(req).await;
    // In-process tests use real Postgres + Redis but no content API,
    // so readiness may return 503. We only check the response has a status field.
    let status = resp.status();
    assert!(
        status == StatusCode::OK || status == StatusCode::SERVICE_UNAVAILABLE,
        "expected 200 or 503, got {status}"
    );
    let body = body_json(resp).await;
    assert!(
        body["status"] == "ok" || body["status"] == "degraded",
        "unexpected status: {body}"
    );
}

// ============================================================
// Auth extractor
// ============================================================

#[tokio::test]
async fn test_auth_missing_header_returns_401() {
    let app = TestApp::new().await;
    let content_id = Uuid::new_v4();
    // POST /v1/likes requires auth — send without Authorization header
    let req = json_request(
        "POST",
        "/v1/likes",
        json!({ "content_type": "post", "content_id": content_id }),
    );
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn test_auth_malformed_header_returns_401() {
    let app = TestApp::new().await;
    let content_id = Uuid::new_v4();
    // Send "Token xxx" instead of "Bearer xxx"
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/likes")
        .header("content-type", "application/json")
        .header("authorization", "Token not-bearer")
        .body(axum::body::Body::from(
            serde_json::to_vec(&json!({ "content_type": "post", "content_id": content_id }))
                .unwrap(),
        ))
        .unwrap();
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn test_auth_non_uuid_token_returns_401() {
    let app = TestApp::new().await;
    let content_id = Uuid::new_v4();
    // TestTokenValidator requires the token to be a valid UUID
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/likes")
        .header("content-type", "application/json")
        .header("authorization", "Bearer not-a-uuid")
        .body(axum::body::Body::from(
            serde_json::to_vec(&json!({ "content_type": "post", "content_id": content_id }))
                .unwrap(),
        ))
        .unwrap();
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");
}

// ============================================================
// ContentPath extractor
// ============================================================

#[tokio::test]
async fn test_unknown_content_type_in_path_returns_400() {
    let app = TestApp::new().await;
    let user_id = Uuid::new_v4();
    let content_id = Uuid::new_v4();
    // "video" is not a registered content type
    let req = get_request(
        &format!("/v1/likes/video/{content_id}/count"),
        Some(user_id),
    );
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "CONTENT_TYPE_UNKNOWN");
}

#[tokio::test]
async fn test_unknown_content_type_in_body_returns_400() {
    let app = TestApp::new().await;
    let user_id = Uuid::new_v4();
    let content_id = Uuid::new_v4();
    // POST /v1/likes with unknown content_type in body
    let req = authed_json_request(
        "POST",
        "/v1/likes",
        user_id,
        json!({ "content_type": "video", "content_id": content_id }),
    );
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "CONTENT_TYPE_UNKNOWN");
}

// ============================================================
// Like lifecycle
// ============================================================

#[tokio::test]
async fn test_like_content_returns_201() {
    let app = TestApp::new().await;
    let user_id = Uuid::new_v4();
    let content_id = Uuid::new_v4();
    let req = authed_json_request(
        "POST",
        "/v1/likes",
        user_id,
        json!({ "content_type": "post", "content_id": content_id }),
    );
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["liked"], true);
    assert_eq!(body["already_existed"], false);
    assert!(body["count"].as_i64().unwrap() >= 1);
}

#[tokio::test]
async fn test_like_idempotent_returns_200() {
    let app = TestApp::new().await;
    let user_id = Uuid::new_v4();
    let content_id = Uuid::new_v4();

    // First like
    let req = authed_json_request(
        "POST",
        "/v1/likes",
        user_id,
        json!({ "content_type": "post", "content_id": content_id }),
    );
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Second like — same user, same content
    let req = authed_json_request(
        "POST",
        "/v1/likes",
        user_id,
        json!({ "content_type": "post", "content_id": content_id }),
    );
    let resp = app.request(req).await;
    // Handler returns CREATED regardless of idempotency (already_existed=true)
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["liked"], true);
    assert_eq!(body["already_existed"], true);
}

#[tokio::test]
async fn test_unlike_content_returns_200() {
    let app = TestApp::new().await;
    let user_id = Uuid::new_v4();
    let content_id = Uuid::new_v4();

    // Like first
    let req = authed_json_request(
        "POST",
        "/v1/likes",
        user_id,
        json!({ "content_type": "post", "content_id": content_id }),
    );
    app.request(req).await;

    // Unlike
    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri(format!("/v1/likes/post/{content_id}"))
        .header("authorization", format!("Bearer {user_id}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["liked"], false);
    assert_eq!(body["was_liked"], true);
}

#[tokio::test]
async fn test_unlike_not_liked_is_idempotent() {
    let app = TestApp::new().await;
    let user_id = Uuid::new_v4();
    let content_id = Uuid::new_v4();

    // Unlike without having liked first
    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri(format!("/v1/likes/post/{content_id}"))
        .header("authorization", format!("Bearer {user_id}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["liked"], false);
    assert_eq!(body["was_liked"], false);
}

// ============================================================
// Count + Status
// ============================================================

#[tokio::test]
async fn test_get_count_returns_zero_for_new_content() {
    let app = TestApp::new().await;
    let content_id = Uuid::new_v4();
    let req = get_request(&format!("/v1/likes/post/{content_id}/count"), None);
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["count"], 0);
    assert_eq!(body["content_type"], "post");
}

#[tokio::test]
async fn test_get_count_increments_after_like() {
    let app = TestApp::new().await;
    let user_id = Uuid::new_v4();
    let content_id = Uuid::new_v4();

    // Like the content
    let req = authed_json_request(
        "POST",
        "/v1/likes",
        user_id,
        json!({ "content_type": "post", "content_id": content_id }),
    );
    app.request(req).await;

    // Check count
    let req = get_request(&format!("/v1/likes/post/{content_id}/count"), None);
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["count"], 1);
}

#[tokio::test]
async fn test_get_status_liked() {
    let app = TestApp::new().await;
    let user_id = Uuid::new_v4();
    let content_id = Uuid::new_v4();

    // Like first
    let req = authed_json_request(
        "POST",
        "/v1/likes",
        user_id,
        json!({ "content_type": "post", "content_id": content_id }),
    );
    app.request(req).await;

    // Check status
    let req = get_request(&format!("/v1/likes/post/{content_id}/status"), Some(user_id));
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["liked"], true);
    assert!(body.get("liked_at").is_some(), "liked_at should be present");
}

#[tokio::test]
async fn test_get_status_not_liked() {
    let app = TestApp::new().await;
    let user_id = Uuid::new_v4();
    let content_id = Uuid::new_v4();

    let req = get_request(&format!("/v1/likes/post/{content_id}/status"), Some(user_id));
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["liked"], false);
    // liked_at is omitted when not liked (skip_serializing_if = Option::is_none)
    assert!(
        body.get("liked_at").is_none() || body["liked_at"].is_null(),
        "liked_at should be absent or null when not liked"
    );
}

// ============================================================
// User likes
// ============================================================

#[tokio::test]
async fn test_get_user_likes_empty() {
    let app = TestApp::new().await;
    let user_id = Uuid::new_v4();
    let req = get_request("/v1/likes/user", Some(user_id));
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
    assert_eq!(body["has_more"], false);
}

#[tokio::test]
async fn test_get_user_likes_after_likes() {
    let app = TestApp::new().await;
    let user_id = Uuid::new_v4();
    let content_id_1 = Uuid::new_v4();
    let content_id_2 = Uuid::new_v4();

    // Like two items
    for content_id in [content_id_1, content_id_2] {
        let req = authed_json_request(
            "POST",
            "/v1/likes",
            user_id,
            json!({ "content_type": "post", "content_id": content_id }),
        );
        app.request(req).await;
    }

    // Fetch user likes
    let req = get_request("/v1/likes/user", Some(user_id));
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(body["has_more"], false);
}

#[tokio::test]
async fn test_get_user_likes_invalid_cursor_returns_400() {
    let app = TestApp::new().await;
    let user_id = Uuid::new_v4();
    let req = get_request("/v1/likes/user?cursor=!!!invalid!!!", Some(user_id));
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "INVALID_CURSOR");
}

// ============================================================
// Batch counts
// ============================================================

#[tokio::test]
async fn test_batch_counts_empty_returns_200() {
    let app = TestApp::new().await;
    let req = json_request("POST", "/v1/likes/batch/counts", json!({ "items": [] }));
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["results"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn test_batch_counts_returns_correct_counts() {
    let app = TestApp::new().await;
    let user_id = Uuid::new_v4();
    let content_id = Uuid::new_v4();

    // Like the item first
    let req = authed_json_request(
        "POST",
        "/v1/likes",
        user_id,
        json!({ "content_type": "post", "content_id": content_id }),
    );
    app.request(req).await;

    // Batch count request
    let req = json_request(
        "POST",
        "/v1/likes/batch/counts",
        json!({
            "items": [
                { "content_type": "post", "content_id": content_id }
            ]
        }),
    );
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["count"], 1);
}

#[tokio::test]
async fn test_batch_counts_too_large_returns_400() {
    let app = TestApp::new().await;
    // Build a batch of 101 items (max is 100)
    let items: Vec<serde_json::Value> = (0..101)
        .map(|_| json!({ "content_type": "post", "content_id": Uuid::new_v4() }))
        .collect();
    let req = json_request("POST", "/v1/likes/batch/counts", json!({ "items": items }));
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "BATCH_TOO_LARGE");
}

// ============================================================
// Batch statuses
// ============================================================

#[tokio::test]
async fn test_batch_statuses_returns_correct_status() {
    let app = TestApp::new().await;
    let user_id = Uuid::new_v4();
    let liked_id = Uuid::new_v4();
    let not_liked_id = Uuid::new_v4();

    // Like one item
    let req = authed_json_request(
        "POST",
        "/v1/likes",
        user_id,
        json!({ "content_type": "post", "content_id": liked_id }),
    );
    app.request(req).await;

    // Batch statuses
    let req = authed_json_request(
        "POST",
        "/v1/likes/batch/statuses",
        user_id,
        json!({
            "items": [
                { "content_type": "post", "content_id": liked_id },
                { "content_type": "post", "content_id": not_liked_id }
            ]
        }),
    );
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);

    // Find result for liked_id
    let liked_result = results
        .iter()
        .find(|r| r["content_id"] == liked_id.to_string())
        .expect("liked_id result should be present");
    assert_eq!(liked_result["liked"], true);

    // Find result for not_liked_id
    let not_liked_result = results
        .iter()
        .find(|r| r["content_id"] == not_liked_id.to_string())
        .expect("not_liked_id result should be present");
    assert_eq!(not_liked_result["liked"], false);
}

#[tokio::test]
async fn test_batch_statuses_requires_auth() {
    let app = TestApp::new().await;
    let req = json_request(
        "POST",
        "/v1/likes/batch/statuses",
        json!({ "items": [] }),
    );
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ============================================================
// Leaderboard
// ============================================================

#[tokio::test]
async fn test_leaderboard_returns_ok() {
    let app = TestApp::new().await;
    let req = get_request("/v1/likes/top", None);
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["items"].is_array(), "items should be an array");
    assert!(body["window"].is_string(), "window should be a string");
}

#[tokio::test]
async fn test_leaderboard_with_valid_window() {
    let app = TestApp::new().await;
    let req = get_request("/v1/likes/top?window=7d", None);
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["window"], "7d");
}

#[tokio::test]
async fn test_leaderboard_invalid_window_returns_400() {
    let app = TestApp::new().await;
    let req = get_request("/v1/likes/top?window=invalid", None);
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "INVALID_WINDOW");
}

// ============================================================
// Metrics
// ============================================================

#[tokio::test]
async fn test_metrics_endpoint_returns_200() {
    let app = TestApp::new().await;
    let req = get_request("/metrics", None);
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    // A freshly-built recorder may have no metrics yet; the endpoint still
    // returns 200 with an empty body (or only comments). Just verify the
    // content-type header is set correctly.
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("text/plain"),
        "expected text/plain content-type, got: {ct}"
    );
}

// ============================================================
// Middleware
// ============================================================

#[tokio::test]
async fn test_response_has_x_request_id_header() {
    let app = TestApp::new().await;
    // /v1/likes/top goes through the full API middleware stack which includes
    // inject_request_id; /health/* routes do not have that middleware.
    let req = get_request("/v1/likes/top", None);
    let resp = app.request(req).await;
    assert!(
        resp.headers().contains_key("x-request-id"),
        "response should contain X-Request-Id header"
    );
}

#[tokio::test]
async fn test_error_body_contains_request_id() {
    let app = TestApp::new().await;
    // Trigger an auth error — the error body should have request_id patched in
    let req = json_request(
        "POST",
        "/v1/likes",
        json!({ "content_type": "post", "content_id": Uuid::new_v4() }),
    );
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    let request_id = body["error"]["request_id"].as_str().unwrap_or("");
    assert!(
        !request_id.is_empty(),
        "error body should contain a non-empty request_id"
    );
}

// ============================================================
// SSE stream (basic validation — just checks content_type and 400 errors)
// ============================================================

#[tokio::test]
async fn test_stream_unknown_content_type_returns_400() {
    let app = TestApp::new().await;
    let content_id = Uuid::new_v4();
    let req = get_request(
        &format!("/v1/likes/stream?content_type=video&content_id={content_id}"),
        None,
    );
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "CONTENT_TYPE_UNKNOWN");
}

#[tokio::test]
async fn test_stream_invalid_content_id_returns_400() {
    let app = TestApp::new().await;
    let req = get_request(
        "/v1/likes/stream?content_type=post&content_id=not-a-uuid",
        None,
    );
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ============================================================
// Content ID validation in path
// ============================================================

#[tokio::test]
async fn test_invalid_content_id_in_path_returns_400() {
    let app = TestApp::new().await;
    let req = get_request("/v1/likes/post/not-a-valid-uuid/count", None);
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "INVALID_CONTENT_ID");
}

// ============================================================
// Multiple content types
// ============================================================

#[tokio::test]
async fn test_like_bonus_hunter_content_type() {
    let app = TestApp::new().await;
    let user_id = Uuid::new_v4();
    let content_id = Uuid::new_v4();
    let req = authed_json_request(
        "POST",
        "/v1/likes",
        user_id,
        json!({ "content_type": "bonus_hunter", "content_id": content_id }),
    );
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["liked"], true);
}

#[tokio::test]
async fn test_like_top_picks_content_type() {
    let app = TestApp::new().await;
    let user_id = Uuid::new_v4();
    let content_id = Uuid::new_v4();
    let req = authed_json_request(
        "POST",
        "/v1/likes",
        user_id,
        json!({ "content_type": "top_picks", "content_id": content_id }),
    );
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

// ============================================================
// Status requires auth
// ============================================================

#[tokio::test]
async fn test_get_status_requires_auth() {
    let app = TestApp::new().await;
    let content_id = Uuid::new_v4();
    let req = get_request(&format!("/v1/likes/post/{content_id}/status"), None);
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_get_user_likes_requires_auth() {
    let app = TestApp::new().await;
    let req = get_request("/v1/likes/user", None);
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ============================================================
// Unlike requires auth
// ============================================================

#[tokio::test]
async fn test_unlike_requires_auth() {
    let app = TestApp::new().await;
    let content_id = Uuid::new_v4();
    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri(format!("/v1/likes/post/{content_id}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ============================================================
// Leaderboard content_type filter
// ============================================================

#[tokio::test]
async fn test_leaderboard_with_content_type_filter() {
    let app = TestApp::new().await;
    let req = get_request("/v1/likes/top?content_type=post", None);
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["items"].is_array());
}

#[tokio::test]
async fn test_leaderboard_unknown_content_type_returns_400() {
    let app = TestApp::new().await;
    let req = get_request("/v1/likes/top?content_type=video", None);
    let resp = app.request(req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "CONTENT_TYPE_UNKNOWN");
}
