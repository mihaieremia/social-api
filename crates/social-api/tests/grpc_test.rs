//! In-process gRPC integration tests.
//!
//! Uses TestApp which boots a real tonic gRPC server on a random port
//! alongside the HTTP router, backed by real Postgres + Redis via testcontainers.
//! Run: cargo test --test grpc_test -- --test-threads=1

mod common;

use common::TestApp;
use social_api::proto::social_v1::{
    BatchCountsRequest, BatchStatusesRequest, ContentRef, GetCountRequest, GetStatusRequest,
    GetUserLikesRequest, HealthCheckRequest, LeaderboardRequest, LikeRequest, PaginationRequest,
    ServingStatus, StreamRequest, TimeWindow, UnlikeRequest, health_client::HealthClient,
    like_service_client::LikeServiceClient,
};
use tokio_stream::StreamExt;
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

// ============================================================
// GetStatus
// ============================================================

#[tokio::test]
async fn grpc_get_status_liked_and_not_liked() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = LikeServiceClient::new(channel);

    let user_id = Uuid::new_v4();
    let content_id = Uuid::new_v4().to_string();

    // Before like: liked=false, no liked_at
    let resp = client
        .get_status(authed(
            GetStatusRequest {
                content_type: "post".to_string(),
                content_id: content_id.clone(),
            },
            user_id,
        ))
        .await
        .expect("get_status before like");

    let body = resp.into_inner();
    assert!(!body.liked);
    assert!(body.liked_at.is_none());

    // Like the content
    client
        .like(authed(
            LikeRequest {
                content_type: "post".to_string(),
                content_id: content_id.clone(),
            },
            user_id,
        ))
        .await
        .expect("like");

    // After like: liked=true, has liked_at
    let resp = client
        .get_status(authed(
            GetStatusRequest {
                content_type: "post".to_string(),
                content_id: content_id.clone(),
            },
            user_id,
        ))
        .await
        .expect("get_status after like");

    let body = resp.into_inner();
    assert!(body.liked);
    assert!(body.liked_at.is_some());
}

// ============================================================
// BatchCounts
// ============================================================

#[tokio::test]
async fn grpc_batch_counts() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = LikeServiceClient::new(channel);

    let user_id = Uuid::new_v4();
    let id1 = Uuid::new_v4().to_string();
    let id2 = Uuid::new_v4().to_string();

    // Like id1 only
    client
        .like(authed(
            LikeRequest {
                content_type: "post".to_string(),
                content_id: id1.clone(),
            },
            user_id,
        ))
        .await
        .expect("like id1");

    // BatchCounts for both
    let resp = client
        .batch_counts(tonic::Request::new(BatchCountsRequest {
            items: vec![
                ContentRef {
                    content_type: "post".to_string(),
                    content_id: id1.clone(),
                },
                ContentRef {
                    content_type: "post".to_string(),
                    content_id: id2.clone(),
                },
            ],
        }))
        .await
        .expect("batch_counts");

    let results = resp.into_inner().results;
    assert_eq!(results.len(), 2);

    let count1 = results.iter().find(|r| r.content_id == id1).expect("id1");
    let count2 = results.iter().find(|r| r.content_id == id2).expect("id2");
    assert_eq!(count1.count, 1);
    assert_eq!(count2.count, 0);
}

// ============================================================
// BatchStatuses
// ============================================================

#[tokio::test]
async fn grpc_batch_statuses() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = LikeServiceClient::new(channel);

    let user_id = Uuid::new_v4();
    let id1 = Uuid::new_v4().to_string();
    let id2 = Uuid::new_v4().to_string();

    // Like id1 only
    client
        .like(authed(
            LikeRequest {
                content_type: "post".to_string(),
                content_id: id1.clone(),
            },
            user_id,
        ))
        .await
        .expect("like id1");

    // BatchStatuses for both
    let resp = client
        .batch_statuses(authed(
            BatchStatusesRequest {
                items: vec![
                    ContentRef {
                        content_type: "post".to_string(),
                        content_id: id1.clone(),
                    },
                    ContentRef {
                        content_type: "post".to_string(),
                        content_id: id2.clone(),
                    },
                ],
            },
            user_id,
        ))
        .await
        .expect("batch_statuses");

    let results = resp.into_inner().results;
    assert_eq!(results.len(), 2);

    let status1 = results.iter().find(|r| r.content_id == id1).expect("id1");
    let status2 = results.iter().find(|r| r.content_id == id2).expect("id2");
    assert!(status1.liked);
    assert!(!status2.liked);
}

#[tokio::test]
async fn grpc_batch_statuses_without_auth() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = LikeServiceClient::new(channel);

    let id = Uuid::new_v4().to_string();
    let status = client
        .batch_statuses(tonic::Request::new(BatchStatusesRequest {
            items: vec![ContentRef {
                content_type: "post".to_string(),
                content_id: id,
            }],
        }))
        .await
        .expect_err("should fail without auth");

    assert_eq!(status.code(), tonic::Code::Unauthenticated);
}

// ============================================================
// GetUserLikes
// ============================================================

#[tokio::test]
async fn grpc_get_user_likes_with_pagination() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = LikeServiceClient::new(channel);

    let user_id = Uuid::new_v4();

    // Like 3 items with delays for ordering
    for _ in 0..3 {
        let content_id = Uuid::new_v4().to_string();
        client
            .like(authed(
                LikeRequest {
                    content_type: "post".to_string(),
                    content_id,
                },
                user_id,
            ))
            .await
            .expect("like");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Page 1: limit=2
    let resp = client
        .get_user_likes(authed(
            GetUserLikesRequest {
                pagination: Some(PaginationRequest {
                    cursor: None,
                    limit: 2,
                }),
                content_type: None,
            },
            user_id,
        ))
        .await
        .expect("get_user_likes page 1");

    let body = resp.into_inner();
    assert_eq!(body.items.len(), 2);
    let pagination = body.pagination.expect("pagination info");
    assert!(pagination.has_more);
    assert!(pagination.next_cursor.is_some());

    // Page 2: use cursor from page 1
    let resp = client
        .get_user_likes(authed(
            GetUserLikesRequest {
                pagination: Some(PaginationRequest {
                    cursor: pagination.next_cursor,
                    limit: 2,
                }),
                content_type: None,
            },
            user_id,
        ))
        .await
        .expect("get_user_likes page 2");

    let body = resp.into_inner();
    assert_eq!(body.items.len(), 1);
    let pagination = body.pagination.expect("pagination info");
    assert!(!pagination.has_more);
}

#[tokio::test]
async fn grpc_get_user_likes_filtered_by_content_type() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = LikeServiceClient::new(channel);

    let user_id = Uuid::new_v4();

    // Like 1 "post"
    client
        .like(authed(
            LikeRequest {
                content_type: "post".to_string(),
                content_id: Uuid::new_v4().to_string(),
            },
            user_id,
        ))
        .await
        .expect("like post");

    // Like 1 "bonus_hunter"
    client
        .like(authed(
            LikeRequest {
                content_type: "bonus_hunter".to_string(),
                content_id: Uuid::new_v4().to_string(),
            },
            user_id,
        ))
        .await
        .expect("like bonus_hunter");

    // Filter by "post" only
    let resp = client
        .get_user_likes(authed(
            GetUserLikesRequest {
                pagination: Some(PaginationRequest {
                    cursor: None,
                    limit: 10,
                }),
                content_type: Some("post".to_string()),
            },
            user_id,
        ))
        .await
        .expect("get_user_likes filtered");

    let body = resp.into_inner();
    assert_eq!(body.items.len(), 1);
    assert_eq!(body.items[0].content_type, "post");
}

// ============================================================
// GetLeaderboard
// ============================================================

#[tokio::test]
async fn grpc_get_leaderboard() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = LikeServiceClient::new(channel);

    let resp = client
        .get_leaderboard(tonic::Request::new(LeaderboardRequest {
            window: TimeWindow::All as i32,
            content_type: None,
            limit: 10,
        }))
        .await
        .expect("get_leaderboard");

    let body = resp.into_inner();
    assert_eq!(body.window, "all");
    // Items may be empty since leaderboard cache may not be populated
}

#[tokio::test]
async fn grpc_get_leaderboard_invalid_window() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = LikeServiceClient::new(channel);

    let status = client
        .get_leaderboard(tonic::Request::new(LeaderboardRequest {
            window: TimeWindow::Unspecified as i32,
            content_type: None,
            limit: 10,
        }))
        .await
        .expect_err("should fail with unspecified window");

    assert_eq!(status.code(), tonic::Code::InvalidArgument);
}

// ============================================================
// StreamLikes
// ============================================================

#[tokio::test]
async fn grpc_stream_likes_receives_heartbeat() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = LikeServiceClient::new(channel);

    let content_id = Uuid::new_v4().to_string();

    let resp = client
        .stream_likes(tonic::Request::new(StreamRequest {
            content_type: "post".to_string(),
            content_id,
        }))
        .await
        .expect("stream_likes");

    let mut stream = resp.into_inner();

    // The heartbeat interval tick fires immediately, so the first event should be a heartbeat
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("timeout waiting for heartbeat")
        .expect("stream ended")
        .expect("stream error");

    match event.event {
        Some(social_api::proto::social_v1::like_event::Event::Heartbeat(hb)) => {
            assert!(hb.timestamp.is_some());
        }
        other => panic!("expected Heartbeat, got {other:?}"),
    }
}

#[tokio::test]
async fn grpc_stream_likes_receives_like_event() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;

    let content_id = Uuid::new_v4().to_string();
    let user_id = Uuid::new_v4();

    // Subscribe to the stream
    let mut stream_client = LikeServiceClient::new(channel.clone());
    let resp = stream_client
        .stream_likes(tonic::Request::new(StreamRequest {
            content_type: "post".to_string(),
            content_id: content_id.clone(),
        }))
        .await
        .expect("stream_likes");

    let mut stream = resp.into_inner();

    // Consume the initial heartbeat (tick fires immediately)
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("timeout waiting for initial heartbeat")
        .expect("stream ended")
        .expect("stream error");

    // Trigger a like from a separate client
    let mut like_client = LikeServiceClient::new(channel);
    like_client
        .like(authed(
            LikeRequest {
                content_type: "post".to_string(),
                content_id: content_id.clone(),
            },
            user_id,
        ))
        .await
        .expect("like");

    // Small delay for pubsub delivery
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Wait for the like event
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("timeout waiting for like event")
        .expect("stream ended")
        .expect("stream error");

    match event.event {
        Some(social_api::proto::social_v1::like_event::Event::Liked(occurred)) => {
            assert_eq!(occurred.content_type, "post");
            assert_eq!(occurred.content_id, content_id);
            assert_eq!(occurred.count, 1);
        }
        other => panic!("expected Liked event, got {other:?}"),
    }
}

// ============================================================
// Edge cases
// ============================================================

#[tokio::test]
async fn grpc_like_idempotent() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = LikeServiceClient::new(channel);

    let user_id = Uuid::new_v4();
    let content_id = Uuid::new_v4().to_string();

    // First like
    client
        .like(authed(
            LikeRequest {
                content_type: "post".to_string(),
                content_id: content_id.clone(),
            },
            user_id,
        ))
        .await
        .expect("first like");

    // Second like (idempotent)
    let resp = client
        .like(authed(
            LikeRequest {
                content_type: "post".to_string(),
                content_id: content_id.clone(),
            },
            user_id,
        ))
        .await
        .expect("second like");

    let body = resp.into_inner();
    assert!(body.liked);
    assert_eq!(body.count, 1);
    assert_eq!(body.already_existed, Some(true));
}

#[tokio::test]
async fn grpc_unlike_when_not_liked() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = LikeServiceClient::new(channel);

    let user_id = Uuid::new_v4();
    let content_id = Uuid::new_v4().to_string();

    // Unlike without prior like
    let resp = client
        .unlike(authed(
            UnlikeRequest {
                content_type: "post".to_string(),
                content_id,
            },
            user_id,
        ))
        .await
        .expect("unlike when not liked");

    let body = resp.into_inner();
    assert!(!body.liked);
    assert_eq!(body.was_liked, Some(false));
}

#[tokio::test]
async fn grpc_get_user_likes_default_pagination() {
    let app = TestApp::new().await;
    let channel = grpc_channel(app.grpc_addr).await;
    let mut client = LikeServiceClient::new(channel);

    let user_id = Uuid::new_v4();

    // GetUserLikes with pagination=None
    let resp = client
        .get_user_likes(authed(
            GetUserLikesRequest {
                pagination: None,
                content_type: None,
            },
            user_id,
        ))
        .await
        .expect("get_user_likes default pagination");

    let body = resp.into_inner();
    let pagination = body.pagination.expect("pagination info");
    assert!(!pagination.has_more);
}
