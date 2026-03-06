//! gRPC `LikeService` implementation.
//!
//! Each RPC method is a thin wrapper that:
//! 1. Extracts/validates request fields
//! 2. Optionally authenticates (write ops + status/user-likes/batch-statuses)
//! 3. Checks rate limits (write ops use token-based, read ops use IP-based)
//! 4. Calls the existing `LikeService` business logic
//! 5. Converts domain response to proto response via the conversion layer
//!
//! Zero business logic duplication -- all logic stays in `LikeService`.

// This module is a building block consumed once the gRPC server is mounted
// (Task 11: Dual Server Mounting). Allow dead_code until that consumer lands.
#![allow(dead_code)]

use std::pin::Pin;
use std::time::{Duration, Instant};

use futures::Stream;
use tonic::{Request, Response, Status};

use crate::grpc::convert::{self, extract_content_refs, from_proto_window, parse_uuid};
use crate::grpc::error::IntoStatus;
use crate::grpc::interceptors::{auth, metrics::record_grpc_request, rate_limit};
use crate::proto::social_v1;
use crate::proto::social_v1::like_service_server::LikeService as LikeServiceProto;
use crate::state::AppState;

/// gRPC implementation of `social.v1.LikeService`.
///
/// Wraps `AppState` and delegates all business logic to the domain `LikeService`.
pub struct GrpcLikeService {
    state: AppState,
}

impl GrpcLikeService {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }

    /// Authenticate the request and return the user.
    async fn authenticate(
        &self,
        metadata: &tonic::metadata::MetadataMap,
    ) -> Result<shared::types::AuthenticatedUser, Status> {
        let token = auth::extract_token(metadata)?;
        auth::validate_token(
            &token,
            self.state.token_validator(),
            self.state.profile_breaker(),
        )
        .await
    }

    /// Check write rate limit using the token from metadata.
    async fn check_write_limit(
        &self,
        metadata: &tonic::metadata::MetadataMap,
    ) -> Result<(), Status> {
        let token = metadata
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        rate_limit::check_grpc_rate_limit(
            self.state.cache(),
            token,
            true,
            self.state.config().rate_limit_write_per_minute,
            self.state.config().rate_limit_read_per_minute,
        )
        .await
    }

    /// Check read rate limit using peer address.
    async fn check_read_limit<T>(&self, req: &Request<T>) -> Result<(), Status> {
        let ip = req
            .remote_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        rate_limit::check_grpc_rate_limit(
            self.state.cache(),
            &ip,
            false,
            self.state.config().rate_limit_write_per_minute,
            self.state.config().rate_limit_read_per_minute,
        )
        .await
    }

    /// Validate content type against config registry.
    #[allow(clippy::result_large_err)]
    fn validate_content_type(&self, content_type: &str) -> Result<(), Status> {
        if !self.state.config().is_valid_content_type(content_type) {
            return Err(Status::invalid_argument(format!(
                "Unknown content type: {content_type}"
            )));
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl LikeServiceProto for GrpcLikeService {
    // ── Like ────────────────────────────────────────────────────────────

    async fn like(
        &self,
        req: Request<social_v1::LikeRequest>,
    ) -> Result<Response<social_v1::LikeResponse>, Status> {
        let start = Instant::now();

        let metadata = req.metadata().clone();
        self.check_write_limit(&metadata).await?;
        let user = self.authenticate(&metadata).await?;

        let inner = req.into_inner();
        self.validate_content_type(&inner.content_type)?;
        let content_id = parse_uuid(&inner.content_id)?;

        let result = self
            .state
            .like_service()
            .like(user.user_id, &inner.content_type, content_id)
            .await
            .into_status()?;

        record_grpc_request("social.v1.LikeService/Like", "OK", start);
        Ok(Response::new(result.into()))
    }

    // ── Unlike ──────────────────────────────────────────────────────────

    async fn unlike(
        &self,
        req: Request<social_v1::UnlikeRequest>,
    ) -> Result<Response<social_v1::LikeResponse>, Status> {
        let start = Instant::now();

        let metadata = req.metadata().clone();
        self.check_write_limit(&metadata).await?;
        let user = self.authenticate(&metadata).await?;

        let inner = req.into_inner();
        self.validate_content_type(&inner.content_type)?;
        let content_id = parse_uuid(&inner.content_id)?;

        let result = self
            .state
            .like_service()
            .unlike(user.user_id, &inner.content_type, content_id)
            .await
            .into_status()?;

        record_grpc_request("social.v1.LikeService/Unlike", "OK", start);
        Ok(Response::new(result.into()))
    }

    // ── GetCount ────────────────────────────────────────────────────────

    async fn get_count(
        &self,
        req: Request<social_v1::GetCountRequest>,
    ) -> Result<Response<social_v1::CountResponse>, Status> {
        let start = Instant::now();

        self.check_read_limit(&req).await?;

        let inner = req.into_inner();
        self.validate_content_type(&inner.content_type)?;
        let content_id = parse_uuid(&inner.content_id)?;

        let result = self
            .state
            .like_service()
            .get_count(&inner.content_type, content_id)
            .await
            .into_status()?;

        record_grpc_request("social.v1.LikeService/GetCount", "OK", start);
        Ok(Response::new(result.into()))
    }

    // ── GetStatus ───────────────────────────────────────────────────────

    async fn get_status(
        &self,
        req: Request<social_v1::GetStatusRequest>,
    ) -> Result<Response<social_v1::StatusResponse>, Status> {
        let start = Instant::now();

        self.check_read_limit(&req).await?;
        let user = self.authenticate(req.metadata()).await?;

        let inner = req.into_inner();
        self.validate_content_type(&inner.content_type)?;
        let content_id = parse_uuid(&inner.content_id)?;

        let result = self
            .state
            .like_service()
            .get_status(user.user_id, &inner.content_type, content_id)
            .await
            .into_status()?;

        let proto = convert::to_proto_status(result, inner.content_type, inner.content_id);

        record_grpc_request("social.v1.LikeService/GetStatus", "OK", start);
        Ok(Response::new(proto))
    }

    // ── BatchCounts ─────────────────────────────────────────────────────

    async fn batch_counts(
        &self,
        req: Request<social_v1::BatchCountsRequest>,
    ) -> Result<Response<social_v1::BatchCountsResponse>, Status> {
        let start = Instant::now();

        self.check_read_limit(&req).await?;

        let inner = req.into_inner();
        let refs = extract_content_refs(&inner.items)?;

        for (ct, _) in &refs {
            self.validate_content_type(ct)?;
        }

        let results = self
            .state
            .like_service()
            .batch_counts(&refs)
            .await
            .into_status()?;

        let proto_results: Vec<social_v1::CountResponse> =
            results.into_iter().map(Into::into).collect();

        record_grpc_request("social.v1.LikeService/BatchCounts", "OK", start);
        Ok(Response::new(social_v1::BatchCountsResponse {
            results: proto_results,
        }))
    }

    // ── BatchStatuses ───────────────────────────────────────────────────

    async fn batch_statuses(
        &self,
        req: Request<social_v1::BatchStatusesRequest>,
    ) -> Result<Response<social_v1::BatchStatusesResponse>, Status> {
        let start = Instant::now();

        self.check_read_limit(&req).await?;
        let user = self.authenticate(req.metadata()).await?;

        let inner = req.into_inner();
        let refs = extract_content_refs(&inner.items)?;

        for (ct, _) in &refs {
            self.validate_content_type(ct)?;
        }

        let results = self
            .state
            .like_service()
            .batch_statuses(user.user_id, &refs)
            .await
            .into_status()?;

        let proto_results: Vec<social_v1::StatusResponse> =
            results.into_iter().map(Into::into).collect();

        record_grpc_request("social.v1.LikeService/BatchStatuses", "OK", start);
        Ok(Response::new(social_v1::BatchStatusesResponse {
            results: proto_results,
        }))
    }

    // ── GetUserLikes ────────────────────────────────────────────────────

    async fn get_user_likes(
        &self,
        req: Request<social_v1::GetUserLikesRequest>,
    ) -> Result<Response<social_v1::UserLikesResponse>, Status> {
        let start = Instant::now();

        self.check_read_limit(&req).await?;
        let user = self.authenticate(req.metadata()).await?;

        let inner = req.into_inner();

        // Optional content type filter
        let content_type_filter = inner.content_type.as_deref();
        if let Some(ct) = content_type_filter {
            self.validate_content_type(ct)?;
        }

        // Extract pagination
        let (cursor, limit) = match &inner.pagination {
            Some(p) => (p.cursor.as_deref(), p.limit as i64),
            None => (None, 20),
        };

        let result = self
            .state
            .like_service()
            .get_user_likes(user.user_id, content_type_filter, cursor, limit)
            .await
            .into_status()?;

        let items: Vec<social_v1::LikeItem> = result.items.into_iter().map(Into::into).collect();

        let pagination = social_v1::PaginationInfo {
            next_cursor: result.next_cursor,
            has_more: result.has_more,
        };

        record_grpc_request("social.v1.LikeService/GetUserLikes", "OK", start);
        Ok(Response::new(social_v1::UserLikesResponse {
            items,
            pagination: Some(pagination),
        }))
    }

    // ── GetLeaderboard ──────────────────────────────────────────────────

    async fn get_leaderboard(
        &self,
        req: Request<social_v1::LeaderboardRequest>,
    ) -> Result<Response<social_v1::LeaderboardResponse>, Status> {
        let start = Instant::now();

        self.check_read_limit(&req).await?;

        let inner = req.into_inner();

        let window = from_proto_window(inner.window).map_err(Status::invalid_argument)?;

        let content_type_filter = inner.content_type.as_deref();
        if let Some(ct) = content_type_filter {
            self.validate_content_type(ct)?;
        }

        let result = self
            .state
            .like_service()
            .get_leaderboard(content_type_filter, window, inner.limit as i64)
            .await
            .into_status()?;

        let items: Vec<social_v1::TopLikedItem> =
            result.items.into_iter().map(Into::into).collect();

        record_grpc_request("social.v1.LikeService/GetLeaderboard", "OK", start);
        Ok(Response::new(social_v1::LeaderboardResponse {
            items,
            window: result.window,
            content_type: result.content_type,
        }))
    }

    // ── StreamLikes ─────────────────────────────────────────────────────

    type StreamLikesStream =
        Pin<Box<dyn Stream<Item = Result<social_v1::LikeEvent, Status>> + Send>>;

    async fn stream_likes(
        &self,
        req: Request<social_v1::StreamRequest>,
    ) -> Result<Response<Self::StreamLikesStream>, Status> {
        let start = Instant::now();

        self.check_read_limit(&req).await?;

        let inner = req.into_inner();
        self.validate_content_type(&inner.content_type)?;
        let _content_id = parse_uuid(&inner.content_id)?;

        let channel = format!("sse:{}:{}", inner.content_type, inner.content_id);
        let content_type = inner.content_type;
        let content_id_str = inner.content_id;

        let heartbeat_secs = self.state.config().sse_heartbeat_interval_secs;
        let shutdown_token = self.state.shutdown_token().clone();
        let pubsub_manager = self.state.pubsub_manager().clone();

        let mut rx = pubsub_manager
            .subscribe(&channel)
            .await
            .map_err(|e| Status::internal(format!("Failed to subscribe to pubsub: {e}")))?;

        record_grpc_request("social.v1.LikeService/StreamLikes", "OK", start);

        let stream = async_stream::stream! {
            let mut heartbeat = tokio::time::interval(Duration::from_secs(heartbeat_secs));

            loop {
                tokio::select! {
                    _ = shutdown_token.cancelled() => {
                        let event = shared::types::LikeEvent::Shutdown {
                            timestamp: chrono::Utc::now(),
                        };
                        let proto: social_v1::LikeEvent = event.into();
                        yield Ok(proto);
                        break;
                    }
                    _ = heartbeat.tick() => {
                        let event = shared::types::LikeEvent::Heartbeat {
                            timestamp: chrono::Utc::now(),
                        };
                        let proto: social_v1::LikeEvent = event.into();
                        yield Ok(proto);
                    }
                    result = rx.recv() => {
                        match result {
                            Ok(payload) => {
                                match serde_json::from_str::<shared::types::LikeEvent>(&payload) {
                                    Ok(domain_event) => {
                                        let mut proto: social_v1::LikeEvent = domain_event.into();
                                        // Fill in content_type and content_id for Liked/Unliked events
                                        if let Some(event) = &mut proto.event {
                                            match event {
                                                social_v1::like_event::Event::Liked(occurred) => {
                                                    occurred.content_type = content_type.clone();
                                                    occurred.content_id = content_id_str.clone();
                                                }
                                                social_v1::like_event::Event::Unliked(occurred) => {
                                                    occurred.content_type = content_type.clone();
                                                    occurred.content_id = content_id_str.clone();
                                                }
                                                _ => {}
                                            }
                                        }
                                        yield Ok(proto);
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            channel = %channel,
                                            "Failed to deserialize LikeEvent from pubsub payload"
                                        );
                                    }
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::debug!(
                                    channel = %channel,
                                    skipped = n,
                                    "gRPC stream client lagged, skipped messages"
                                );
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                // Bridge task ended
                                let event = shared::types::LikeEvent::Shutdown {
                                    timestamp: chrono::Utc::now(),
                                };
                                let proto: social_v1::LikeEvent = event.into();
                                yield Ok(proto);
                                break;
                            }
                        }
                    }
                }
            }
        };

        Ok(Response::new(Box::pin(stream) as Self::StreamLikesStream))
    }
}
