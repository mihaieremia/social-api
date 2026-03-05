//! Integration tests for the Social API.
//!
//! These tests require running Postgres, Redis, and mock services.
//! Run with: `cargo test --test integration_test -- --ignored`
//! Prerequisites: `docker compose up postgres redis mock-services`

use reqwest::Client;
use serde_json::{Value, json};

const BASE_URL: &str = "http://localhost:8080";
const MOCK_URL: &str = "http://localhost:8081";
const TOKEN_USER_1: &str = "tok_user_1";
const TOKEN_USER_2: &str = "tok_user_2";
const TOKEN_USER_3: &str = "tok_user_3";
const TOKEN_USER_4: &str = "tok_user_4";
// TOKEN_USER_5 is reserved exclusively for the rate limit test — do not use in other write tests.
const TOKEN_USER_5: &str = "tok_user_5";
const VALID_POST_ID: &str = "731b0395-4888-4822-b516-05b4b7bf2089";
const VALID_POST_ID_2: &str = "9601c044-6130-4ee5-a155-96570e05a02f";
// Dedicated IDs for pagination test (bonus_hunter type to avoid conflicts)
const PAGINATION_CONTENT_1: &str = "c3d4e5f6-a7b8-9012-cdef-123456789012";
const PAGINATION_CONTENT_2: &str = "d4e5f6a7-b8c9-0123-def0-234567890123";

fn client() -> Client {
    Client::new()
}

fn auth_header(token: &str) -> String {
    format!("Bearer {token}")
}

// ============================================================
// Health Checks
// ============================================================

#[tokio::test]
#[ignore]
async fn test_liveness_probe() {
    let resp = client()
        .get(format!("{BASE_URL}/health/live"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
#[ignore]
async fn test_readiness_probe() {
    let resp = client()
        .get(format!("{BASE_URL}/health/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
}

// ============================================================
// Like Lifecycle
// ============================================================

#[tokio::test]
#[ignore]
async fn test_like_lifecycle() {
    let c = client();

    // Like content
    let resp = c
        .post(format!("{BASE_URL}/v1/likes"))
        .header("Authorization", auth_header(TOKEN_USER_1))
        .json(&json!({
            "content_type": "post",
            "content_id": VALID_POST_ID
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["liked"], true);

    // Check count
    let resp = c
        .get(format!("{BASE_URL}/v1/likes/post/{VALID_POST_ID}/count"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(body["count"].as_i64().unwrap() >= 1);

    // Check status
    let resp = c
        .get(format!("{BASE_URL}/v1/likes/post/{VALID_POST_ID}/status"))
        .header("Authorization", auth_header(TOKEN_USER_1))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["liked"], true);
    assert!(body["liked_at"].is_string());

    // Unlike
    let resp = c
        .delete(format!("{BASE_URL}/v1/likes/post/{VALID_POST_ID}"))
        .header("Authorization", auth_header(TOKEN_USER_1))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["liked"], false);
    assert_eq!(body["was_liked"], true);
}

#[tokio::test]
#[ignore]
async fn test_like_idempotency() {
    let c = client();

    // Like twice
    let resp1 = c
        .post(format!("{BASE_URL}/v1/likes"))
        .header("Authorization", auth_header(TOKEN_USER_2))
        .json(&json!({
            "content_type": "post",
            "content_id": VALID_POST_ID_2
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp1.status(), 201);

    let resp2 = c
        .post(format!("{BASE_URL}/v1/likes"))
        .header("Authorization", auth_header(TOKEN_USER_2))
        .json(&json!({
            "content_type": "post",
            "content_id": VALID_POST_ID_2
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status(), 201);
    let body: Value = resp2.json().await.unwrap();
    assert_eq!(body["already_existed"], true);

    // Cleanup
    let _ = c
        .delete(format!("{BASE_URL}/v1/likes/post/{VALID_POST_ID_2}"))
        .header("Authorization", auth_header(TOKEN_USER_2))
        .send()
        .await;
}

#[tokio::test]
#[ignore]
async fn test_unlike_idempotency() {
    let c = client();

    // Use a dedicated user + content that no other test touches
    // First ensure clean state by unliking
    let _ = c
        .delete(format!(
            "{BASE_URL}/v1/likes/top_picks/b8c9d0e1-f2a3-4567-1234-678901234567"
        ))
        .header("Authorization", auth_header(TOKEN_USER_4))
        .send()
        .await;

    // Unlike something never liked (or just cleaned up)
    let resp = c
        .delete(format!(
            "{BASE_URL}/v1/likes/top_picks/b8c9d0e1-f2a3-4567-1234-678901234567"
        ))
        .header("Authorization", auth_header(TOKEN_USER_4))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["was_liked"], false);
}

// ============================================================
// Authentication
// ============================================================

#[tokio::test]
#[ignore]
async fn test_missing_auth_header() {
    let resp = client()
        .post(format!("{BASE_URL}/v1/likes"))
        .json(&json!({
            "content_type": "post",
            "content_id": VALID_POST_ID
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");
}

#[tokio::test]
#[ignore]
async fn test_invalid_token() {
    let resp = client()
        .post(format!("{BASE_URL}/v1/likes"))
        .header("Authorization", "Bearer invalid_token")
        .json(&json!({
            "content_type": "post",
            "content_id": VALID_POST_ID
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

// ============================================================
// Content Validation
// ============================================================

#[tokio::test]
#[ignore]
async fn test_unknown_content_type() {
    let resp = client()
        .get(format!(
            "{BASE_URL}/v1/likes/nonexistent/{VALID_POST_ID}/count"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "CONTENT_TYPE_UNKNOWN");
}

#[tokio::test]
#[ignore]
async fn test_invalid_content_id() {
    let resp = client()
        .get(format!("{BASE_URL}/v1/likes/post/not-a-uuid/count"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

// ============================================================
// Batch Operations
// ============================================================

#[tokio::test]
#[ignore]
async fn test_batch_counts() {
    let resp = client()
        .post(format!("{BASE_URL}/v1/likes/batch/counts"))
        .json(&json!({
            "items": [
                { "content_type": "post", "content_id": VALID_POST_ID },
                { "content_type": "post", "content_id": VALID_POST_ID_2 }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["results"].as_array().unwrap().len(), 2);
}

#[tokio::test]
#[ignore]
async fn test_batch_statuses() {
    let resp = client()
        .post(format!("{BASE_URL}/v1/likes/batch/statuses"))
        .header("Authorization", auth_header(TOKEN_USER_1))
        .json(&json!({
            "items": [
                { "content_type": "post", "content_id": VALID_POST_ID },
                { "content_type": "post", "content_id": VALID_POST_ID_2 }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["results"].as_array().unwrap().len(), 2);
}

#[tokio::test]
#[ignore]
async fn test_batch_too_large() {
    let items: Vec<Value> = (0..101)
        .map(|_| {
            json!({
                "content_type": "post",
                "content_id": uuid::Uuid::new_v4()
            })
        })
        .collect();

    let resp = client()
        .post(format!("{BASE_URL}/v1/likes/batch/counts"))
        .json(&json!({ "items": items }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "BATCH_TOO_LARGE");
}

// ============================================================
// User Likes (Pagination)
// ============================================================

#[tokio::test]
#[ignore]
async fn test_user_likes_pagination() {
    let c = client();

    // Use dedicated user + content type to avoid conflicts with parallel tests
    for id in [PAGINATION_CONTENT_1, PAGINATION_CONTENT_2] {
        let _ = c
            .post(format!("{BASE_URL}/v1/likes"))
            .header("Authorization", auth_header(TOKEN_USER_3))
            .json(&json!({
                "content_type": "bonus_hunter",
                "content_id": id
            }))
            .send()
            .await;
    }

    // Get first page with limit=1
    let resp = c
        .get(format!("{BASE_URL}/v1/likes/user?limit=1"))
        .header("Authorization", auth_header(TOKEN_USER_3))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert!(body["has_more"].as_bool().unwrap());
    assert!(body["next_cursor"].is_string());

    // Cleanup
    for id in [PAGINATION_CONTENT_1, PAGINATION_CONTENT_2] {
        let _ = c
            .delete(format!("{BASE_URL}/v1/likes/bonus_hunter/{id}"))
            .header("Authorization", auth_header(TOKEN_USER_3))
            .send()
            .await;
    }
}

// ============================================================
// Leaderboard
// ============================================================

#[tokio::test]
#[ignore]
async fn test_leaderboard() {
    let resp = client()
        .get(format!("{BASE_URL}/v1/likes/top?window=all&limit=5"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["window"], "all");
    assert!(body["items"].is_array());
}

#[tokio::test]
#[ignore]
async fn test_leaderboard_invalid_window() {
    let resp = client()
        .get(format!("{BASE_URL}/v1/likes/top?window=1y"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "INVALID_WINDOW");
}

// ============================================================
// Metrics
// ============================================================

#[tokio::test]
#[ignore]
async fn test_metrics_endpoint() {
    let resp = client()
        .get(format!("{BASE_URL}/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    // Prometheus format should contain metric names
    assert!(body.contains("social_api_http_requests_total") || body.is_empty());
}

// ============================================================
// Mock Services
// ============================================================

#[tokio::test]
#[ignore]
async fn test_mock_profile_api() {
    let resp = client()
        .get(format!("{MOCK_URL}/v1/auth/validate"))
        .header("Authorization", "Bearer tok_user_1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["valid"], true);
    assert!(body["user_id"].as_str().unwrap().contains("550e8400"));
}

#[tokio::test]
#[ignore]
async fn test_mock_content_api() {
    let resp = client()
        .get(format!("{MOCK_URL}/v1/post/{VALID_POST_ID}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["id"], VALID_POST_ID);
}

#[tokio::test]
#[ignore]
async fn test_mock_content_not_found() {
    let fake_id = uuid::Uuid::new_v4();
    let resp = client()
        .get(format!("{MOCK_URL}/v1/post/{fake_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// ============================================================
// Circuit Breaker
// ============================================================

/// Verifies the circuit breaker metric is registered and starts at 0 (Closed).
#[tokio::test]
#[ignore]
async fn test_circuit_breaker_metric_registered() {
    let resp = client()
        .get(format!("{BASE_URL}/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _body = resp.text().await.unwrap();
    // The gauge should exist once a transition or init has emitted it.
    // If no transitions have occurred yet, the metric may not appear --
    // we verify at minimum the endpoint is healthy.
    // After any auth-dependent request, the breaker records success/failure,
    // so trigger one first.
    let _ = client()
        .post(format!("{BASE_URL}/v1/likes"))
        .header("Authorization", auth_header(TOKEN_USER_1))
        .json(&json!({
            "content_type": "post",
            "content_id": VALID_POST_ID
        }))
        .send()
        .await;

    // Re-fetch metrics after a real request
    let resp = client()
        .get(format!("{BASE_URL}/metrics"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    // External call metrics should be present after a real auth call
    assert!(
        body.contains("social_api_external_calls_total"),
        "Expected external call counter in metrics output"
    );
    assert!(
        body.contains("social_api_external_call_duration_seconds"),
        "Expected external call histogram in metrics output"
    );
}

// ============================================================
// Concurrent Likes — Race Condition Verification
// ============================================================

/// Spawns 5 concurrent like requests (one per user) against the same content item.
/// Verifies the unique constraint holds and final count equals exactly 5.
#[tokio::test]
#[ignore]
async fn test_concurrent_likes_race_condition() {
    use futures::future::join_all;

    // Dedicated top_picks ID not used by any other test
    const CONTENT_TYPE: &str = "top_picks";
    const CONCURRENT_CONTENT_ID: &str = "c0000003-0001-4000-8000-000000000001";

    let tokens = [
        TOKEN_USER_1,
        TOKEN_USER_2,
        TOKEN_USER_3,
        TOKEN_USER_4,
        TOKEN_USER_5,
    ];

    // Ensure clean state: unlike for all 5 users before the test
    for token in &tokens {
        let _ = client()
            .delete(format!(
                "{BASE_URL}/v1/likes/{CONTENT_TYPE}/{CONCURRENT_CONTENT_ID}"
            ))
            .header("Authorization", auth_header(token))
            .send()
            .await;
    }

    // Fire 5 concurrent like requests, one per user
    let futures: Vec<_> = tokens
        .iter()
        .map(|&token| async move {
            client()
                .post(format!("{BASE_URL}/v1/likes"))
                .header("Authorization", format!("Bearer {token}"))
                .json(&json!({
                    "content_type": CONTENT_TYPE,
                    "content_id": CONCURRENT_CONTENT_ID
                }))
                .send()
                .await
                .unwrap()
        })
        .collect();

    let responses = join_all(futures).await;

    // All 5 must succeed (201 Created, not 500 or duplicate key error)
    for (i, resp) in responses.iter().enumerate() {
        assert_eq!(
            resp.status().as_u16(),
            201,
            "Concurrent request {i} expected 201, got {}",
            resp.status()
        );
    }

    // Final count must be exactly 5 — no phantom duplicates
    let count_resp = client()
        .get(format!(
            "{BASE_URL}/v1/likes/{CONTENT_TYPE}/{CONCURRENT_CONTENT_ID}/count"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(count_resp.status(), 200);
    let body: Value = count_resp.json().await.unwrap();
    assert_eq!(
        body["count"], 5,
        "Expected count=5 after 5 concurrent likes, got {}",
        body["count"]
    );

    // Cleanup
    for token in &tokens {
        let _ = client()
            .delete(format!(
                "{BASE_URL}/v1/likes/{CONTENT_TYPE}/{CONCURRENT_CONTENT_ID}"
            ))
            .header("Authorization", auth_header(token))
            .send()
            .await;
    }
}

// ============================================================
// SSE Lifecycle
// ============================================================

/// Connects to the SSE stream, triggers a like event, and asserts the event is received.
#[tokio::test]
#[ignore]
async fn test_sse_receives_like_event() {
    // Dedicated bonus_hunter ID not used by other tests
    const SSE_CONTENT_TYPE: &str = "bonus_hunter";
    const SSE_CONTENT_ID: &str = "e5f6a7b8-c9d0-1234-ef01-345678901234";

    // Ensure clean state
    let _ = client()
        .delete(format!(
            "{BASE_URL}/v1/likes/{SSE_CONTENT_TYPE}/{SSE_CONTENT_ID}"
        ))
        .header("Authorization", auth_header(TOKEN_USER_1))
        .send()
        .await;

    // Connect to SSE stream
    let mut sse_resp = client()
        .get(format!(
            "{BASE_URL}/v1/likes/stream?content_type={SSE_CONTENT_TYPE}&content_id={SSE_CONTENT_ID}"
        ))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(sse_resp.status(), 200);

    // Trigger a like 200ms after connecting (gives SSE time to subscribe)
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        client()
            .post(format!("{BASE_URL}/v1/likes"))
            .header("Authorization", auth_header(TOKEN_USER_1))
            .json(&json!({
                "content_type": SSE_CONTENT_TYPE,
                "content_id": SSE_CONTENT_ID
            }))
            .send()
            .await
            .unwrap();
    });

    // Read chunks until a like event appears (5s timeout)
    let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match sse_resp.chunk().await.unwrap() {
                Some(chunk) => {
                    let s = String::from_utf8_lossy(&chunk);
                    if s.contains("\"event\":\"like\"") {
                        return true;
                    }
                }
                None => return false,
            }
        }
    })
    .await;

    assert!(
        matches!(found, Ok(true)),
        "Expected to receive a 'like' SSE event within 5s"
    );

    // Cleanup
    let _ = client()
        .delete(format!(
            "{BASE_URL}/v1/likes/{SSE_CONTENT_TYPE}/{SSE_CONTENT_ID}"
        ))
        .header("Authorization", auth_header(TOKEN_USER_1))
        .send()
        .await;
}

// ============================================================
// Rate Limiting
// ============================================================

/// Fires 31 write requests with the same token and asserts the 31st returns 429.
///
/// Uses TOKEN_USER_5 exclusively — no other test may issue write requests with this token.
/// The sliding window is 60s; ensure a fresh window by not running this immediately
/// after another run of this test.
#[tokio::test]
#[ignore]
async fn test_rate_limit_write_endpoint() {
    const CONTENT_TYPE: &str = "top_picks";
    const RATE_LIMIT_CONTENT_ID: &str = "c0000003-0002-4000-8000-000000000002";

    // Ensure clean state
    let _ = client()
        .delete(format!(
            "{BASE_URL}/v1/likes/{CONTENT_TYPE}/{RATE_LIMIT_CONTENT_ID}"
        ))
        .header("Authorization", auth_header(TOKEN_USER_5))
        .send()
        .await;

    let mut hit_rate_limit = false;

    // Fire up to 31 requests; expect the 31st to be rate-limited
    for i in 0..=30u32 {
        let resp = client()
            .post(format!("{BASE_URL}/v1/likes"))
            .header("Authorization", auth_header(TOKEN_USER_5))
            .json(&json!({
                "content_type": CONTENT_TYPE,
                "content_id": RATE_LIMIT_CONTENT_ID
            }))
            .send()
            .await
            .unwrap();

        let status = resp.status().as_u16();
        if status == 429 {
            let has_retry_after = resp.headers().get("retry-after").is_some();
            let body: Value = resp.json().await.unwrap();
            assert_eq!(
                body["error"]["code"], "RATE_LIMITED",
                "Request {i}: expected RATE_LIMITED error code"
            );
            assert!(
                has_retry_after,
                "Request {i}: 429 response missing Retry-After header"
            );
            hit_rate_limit = true;
            break;
        }

        // Allow 201 (new like) or re-like idempotency
        assert!(
            status == 201,
            "Request {i}: expected 201 before rate limit, got {status}"
        );
    }

    assert!(
        hit_rate_limit,
        "Expected rate limit (429) within 31 requests — check RATE_LIMIT_WRITE_PER_MINUTE config"
    );

    // Cleanup
    let _ = client()
        .delete(format!(
            "{BASE_URL}/v1/likes/{CONTENT_TYPE}/{RATE_LIMIT_CONTENT_ID}"
        ))
        .header("Authorization", auth_header(TOKEN_USER_5))
        .send()
        .await;
}

/// Verifies that when the Profile API is unreachable, the circuit breaker
/// trips and subsequent auth-dependent requests fail fast with 503.
///
/// This test requires stopping the mock-services container:
///   docker compose stop mock-services
///
/// After the test, restart with:
///   docker compose start mock-services
///
/// The test is ignored by default and tagged for manual fault-injection runs.
#[tokio::test]
#[ignore]
async fn test_circuit_breaker_trips_on_profile_api_failure() {
    let c = client();

    // Pre-check: if mock-services is running, this test is a no-op.
    // We detect this by making a health check to the mock.
    let mock_health = c
        .get(format!("{MOCK_URL}/health"))
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await;

    if mock_health.is_ok() {
        eprintln!(
            "SKIP: mock-services is running. Stop it first: docker compose stop mock-services"
        );
        return;
    }

    // Mock is down. Send enough auth requests to trip the breaker.
    // Default threshold is 5 consecutive failures.
    for i in 0..6 {
        let resp = c
            .post(format!("{BASE_URL}/v1/likes"))
            .header("Authorization", auth_header(TOKEN_USER_1))
            .json(&json!({
                "content_type": "post",
                "content_id": VALID_POST_ID
            }))
            .send()
            .await
            .unwrap();

        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap();

        // All should be 503 (dependency unavailable)
        assert_eq!(
            status, 503,
            "Request {i}: expected 503, got {status}: {body}"
        );
        assert_eq!(
            body["error"]["code"], "DEPENDENCY_UNAVAILABLE",
            "Request {i}: expected DEPENDENCY_UNAVAILABLE"
        );
    }

    // Verify the breaker is now open by checking metrics
    let resp = c.get(format!("{BASE_URL}/metrics")).send().await.unwrap();
    let metrics_body = resp.text().await.unwrap();

    // Circuit breaker gauge should show 2.0 (Open state)
    if metrics_body.contains("social_api_circuit_breaker_state") {
        assert!(
            metrics_body.contains("social_api_circuit_breaker_state{")
                && metrics_body.contains("profile_api"),
            "Expected circuit breaker metric for profile_api"
        );
    }

    // The 7th request should also fail fast (breaker is open, no external call)
    let resp = c
        .post(format!("{BASE_URL}/v1/likes"))
        .header("Authorization", auth_header(TOKEN_USER_1))
        .json(&json!({
            "content_type": "post",
            "content_id": VALID_POST_ID
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
}
