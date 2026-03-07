//! In-process gRPC integration tests.
//!
//! Uses TestApp which boots a real tonic gRPC server on a random port
//! alongside the HTTP router, backed by real Postgres + Redis via testcontainers.
//! Run: cargo test --test grpc_test -- --test-threads=1

mod common;

use common::TestApp;
use social_api::proto::social_v1::{
    GetCountRequest, HealthCheckRequest, LikeRequest, ServingStatus, UnlikeRequest,
    health_client::HealthClient, like_service_client::LikeServiceClient,
};
use uuid::Uuid;

// ── Helpers ──────────────────────────────────────────────────────────────────

async fn grpc_channel(addr: std::net::SocketAddr) -> tonic::transport::Channel {
    tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("valid uri")
        .connect()
        .await
        .expect("grpc connect")
}

fn authed<T>(msg: T, user_id: Uuid) -> tonic::Request<T> {
    let mut req = tonic::Request::new(msg);
    req.metadata_mut().insert(
        "authorization",
        format!("Bearer {user_id}").parse().unwrap(),
    );
    req
}

// ============================================================
// Health
// ============================================================

#[tokio::test]
async fn grpc_health_check_serving() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = HealthClient::new(channel);

    let resp = client
        .check(tonic::Request::new(HealthCheckRequest {
            service: String::new(),
        }))
        .await
        .expect("health check");

    let body = resp.into_inner();
    assert_eq!(body.status, ServingStatus::Serving as i32);
    assert_eq!(body.details.get("database").map(String::as_str), Some("ok"));
    assert_eq!(body.details.get("redis").map(String::as_str), Some("ok"));
}

// ============================================================
// GetCount
// ============================================================

#[tokio::test]
async fn grpc_get_count_returns_zero_for_new_content() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = LikeServiceClient::new(channel);

    let content_id = Uuid::new_v4().to_string();
    let resp = client
        .get_count(tonic::Request::new(GetCountRequest {
            content_type: "post".to_string(),
            content_id,
        }))
        .await
        .expect("get_count");

    assert_eq!(resp.into_inner().count, 0);
}

#[tokio::test]
async fn grpc_get_count_invalid_uuid() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = LikeServiceClient::new(channel);

    let status = client
        .get_count(tonic::Request::new(GetCountRequest {
            content_type: "post".to_string(),
            content_id: "not-a-uuid".to_string(),
        }))
        .await
        .expect_err("should fail with invalid uuid");

    assert_eq!(status.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn grpc_get_count_unknown_content_type() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = LikeServiceClient::new(channel);

    let content_id = Uuid::new_v4().to_string();
    let status = client
        .get_count(tonic::Request::new(GetCountRequest {
            content_type: "nonexistent".to_string(),
            content_id,
        }))
        .await
        .expect_err("should fail with unknown content type");

    assert_eq!(status.code(), tonic::Code::InvalidArgument);
}

// ============================================================
// Like / Unlike cycle
// ============================================================

#[tokio::test]
async fn grpc_like_and_unlike_cycle() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut like_client = LikeServiceClient::new(channel.clone());
    let mut count_client = LikeServiceClient::new(channel);

    let user_id = Uuid::new_v4();
    let content_id = Uuid::new_v4().to_string();

    // Like
    let like_resp = like_client
        .like(authed(
            LikeRequest {
                content_type: "post".to_string(),
                content_id: content_id.clone(),
            },
            user_id,
        ))
        .await
        .expect("like");

    let like_body = like_resp.into_inner();
    assert!(like_body.liked);
    assert_eq!(like_body.count, 1);

    // Verify count = 1
    let count_resp = count_client
        .get_count(tonic::Request::new(GetCountRequest {
            content_type: "post".to_string(),
            content_id: content_id.clone(),
        }))
        .await
        .expect("get_count after like");

    assert_eq!(count_resp.into_inner().count, 1);

    // Unlike
    let unlike_resp = like_client
        .unlike(authed(
            UnlikeRequest {
                content_type: "post".to_string(),
                content_id: content_id.clone(),
            },
            user_id,
        ))
        .await
        .expect("unlike");

    let unlike_body = unlike_resp.into_inner();
    assert!(!unlike_body.liked);
    assert_eq!(unlike_body.count, 0);

    // Verify count = 0
    let count_resp = count_client
        .get_count(tonic::Request::new(GetCountRequest {
            content_type: "post".to_string(),
            content_id,
        }))
        .await
        .expect("get_count after unlike");

    assert_eq!(count_resp.into_inner().count, 0);
}

// ============================================================
// Auth
// ============================================================

#[tokio::test]
async fn grpc_like_without_auth_returns_unauthenticated() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = LikeServiceClient::new(channel);

    let content_id = Uuid::new_v4().to_string();
    let status = client
        .like(tonic::Request::new(LikeRequest {
            content_type: "post".to_string(),
            content_id,
        }))
        .await
        .expect_err("should fail without auth");

    assert_eq!(status.code(), tonic::Code::Unauthenticated);
}
