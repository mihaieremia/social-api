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

use std::pin::Pin;
use std::time::Instant;

use futures::{Stream, StreamExt};
use tonic::{Code, Request, Response, Status};

use crate::content;
use crate::grpc::convert::{self, extract_content_refs, from_proto_window, parse_uuid};
use crate::grpc::error::IntoStatus;
use crate::grpc::interceptors::auth;
use crate::grpc::interceptors::metrics::{grpc_code_label, record_grpc_request};
use crate::grpc::interceptors::rate_limit;
use crate::middleware::rate_limit as mw_rate_limit;
use crate::proto::social_v1;
use crate::proto::social_v1::like_service_server::LikeService as LikeServiceProto;
use crate::realtime::{self as streaming, LikeStreamItem};
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

    /// Check per-user write rate limit. Called AFTER authenticate().
    async fn check_user_write_limit(&self, user_id: uuid::Uuid) -> Result<(), Status> {
        mw_rate_limit::enforce_user_write_limit(
            self.state.cache(),
            user_id,
            self.state.config().rate_limit_write_per_minute,
        )
        .await
        .map_err(|e| {
            if let shared::errors::AppError::RateLimited { retry_after_secs } = e {
                Status::resource_exhausted(format!(
                    "Rate limit exceeded. Retry after {retry_after_secs}s"
                ))
            } else {
                Status::internal("Rate limit check failed")
            }
        })
    }

    /// Check per-user read rate limit. Called AFTER authenticate().
    async fn check_user_read_limit(&self, user_id: uuid::Uuid) -> Result<(), Status> {
        mw_rate_limit::enforce_user_read_limit(
            self.state.cache(),
            user_id,
            self.state.config().rate_limit_read_per_minute,
        )
        .await
        .map_err(|e| {
            if let shared::errors::AppError::RateLimited { retry_after_secs } = e {
                Status::resource_exhausted(format!(
                    "Rate limit exceeded. Retry after {retry_after_secs}s"
                ))
            } else {
                Status::internal("Rate limit check failed")
            }
        })
    }

    /// Check public read rate limit using forwarded headers or peer address.
    async fn check_public_read_limit<T>(&self, req: &Request<T>) -> Result<(), Status> {
        let ip = Self::extract_client_ip(req);
        rate_limit::check_grpc_rate_limit(
            self.state.cache(),
            &ip,
            false,
            self.state.config().rate_limit_write_per_minute,
            self.state.config().rate_limit_read_per_minute,
        )
        .await
    }

    /// Extract client IP from gRPC metadata, preferring forwarded headers.
    fn extract_client_ip<T>(req: &Request<T>) -> String {
        // Prefer x-forwarded-for (leftmost = original client address)
        if let Some(xff) = req.metadata().get("x-forwarded-for")
            && let Ok(val) = xff.to_str()
        {
            let ip = mw_rate_limit::extract_real_ip(val);
            if ip != "unknown" {
                return ip.to_string();
            }
        }
        // Fallback: x-real-ip
        if let Some(xri) = req.metadata().get("x-real-ip")
            && let Ok(val) = xri.to_str()
        {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        // Fallback: peer socket address
        req.remote_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// Validate content type against config registry.
    #[allow(clippy::result_large_err)]
    fn validate_content_type(&self, content_type: &str) -> Result<(), Status> {
        content::ensure_registered_content_type(self.state.config(), content_type).into_status()
    }

    #[allow(clippy::result_large_err)]
    fn finish_request<T>(
        method: &'static str,
        start: Instant,
        result: Result<Response<T>, Status>,
    ) -> Result<Response<T>, Status> {
        match result {
            Ok(response) => {
                record_grpc_request(method, grpc_code_label(Code::Ok), start);
                Ok(response)
            }
            Err(status) => {
                record_grpc_request(method, grpc_code_label(status.code()), start);
                Err(status)
            }
        }
    }
}

#[tonic::async_trait]
impl LikeServiceProto for GrpcLikeService {
    // ── Like ────────────────────────────────────────────────────────────

    async fn like(
        &self,
        req: Request<social_v1::LikeRequest>,
    ) -> Result<Response<social_v1::LikeResponse>, Status> {
        const METHOD: &str = "social.v1.LikeService/Like";
        let start = Instant::now();

        let result = async {
            let metadata = req.metadata().clone();
            let user = self.authenticate(&metadata).await?;
            self.check_user_write_limit(user.user_id).await?;

            let inner = req.into_inner();
            self.validate_content_type(&inner.content_type)?;
            let content_id = parse_uuid(&inner.content_id)?;

            let result = self
                .state
                .like_service()
                .like(user.user_id, &inner.content_type, content_id)
                .await
                .into_status()?;

            Ok(Response::new(result.into()))
        }
        .await;

        Self::finish_request(METHOD, start, result)
    }

    // ── Unlike ──────────────────────────────────────────────────────────

    async fn unlike(
        &self,
        req: Request<social_v1::UnlikeRequest>,
    ) -> Result<Response<social_v1::LikeResponse>, Status> {
        const METHOD: &str = "social.v1.LikeService/Unlike";
        let start = Instant::now();

        let result = async {
            let metadata = req.metadata().clone();
            let user = self.authenticate(&metadata).await?;
            self.check_user_write_limit(user.user_id).await?;

            let inner = req.into_inner();
            self.validate_content_type(&inner.content_type)?;
            let content_id = parse_uuid(&inner.content_id)?;

            let result = self
                .state
                .like_service()
                .unlike(user.user_id, &inner.content_type, content_id)
                .await
                .into_status()?;

            Ok(Response::new(result.into()))
        }
        .await;

        Self::finish_request(METHOD, start, result)
    }

    // ── GetCount ────────────────────────────────────────────────────────

    async fn get_count(
        &self,
        req: Request<social_v1::GetCountRequest>,
    ) -> Result<Response<social_v1::CountResponse>, Status> {
        const METHOD: &str = "social.v1.LikeService/GetCount";
        let start = Instant::now();

        let result = async {
            self.check_public_read_limit(&req).await?;

            let inner = req.into_inner();
            self.validate_content_type(&inner.content_type)?;
            let content_id = parse_uuid(&inner.content_id)?;

            let result = self
                .state
                .like_service()
                .get_count(&inner.content_type, content_id)
                .await
                .into_status()?;

            Ok(Response::new(result.into()))
        }
        .await;

        Self::finish_request(METHOD, start, result)
    }

    // ── GetStatus ───────────────────────────────────────────────────────

    async fn get_status(
        &self,
        req: Request<social_v1::GetStatusRequest>,
    ) -> Result<Response<social_v1::StatusResponse>, Status> {
        const METHOD: &str = "social.v1.LikeService/GetStatus";
        let start = Instant::now();

        let result = async {
            let user = self.authenticate(req.metadata()).await?;
            self.check_user_read_limit(user.user_id).await?;

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

            Ok(Response::new(proto))
        }
        .await;

        Self::finish_request(METHOD, start, result)
    }

    // ── BatchCounts ─────────────────────────────────────────────────────

    async fn batch_counts(
        &self,
        req: Request<social_v1::BatchCountsRequest>,
    ) -> Result<Response<social_v1::BatchCountsResponse>, Status> {
        const METHOD: &str = "social.v1.LikeService/BatchCounts";
        let start = Instant::now();

        let result = async {
            self.check_public_read_limit(&req).await?;

            let inner = req.into_inner();
            let refs = extract_content_refs(&inner.items)?;

            content::ensure_registered_content_types(
                self.state.config(),
                refs.iter().map(|(ct, _)| ct.as_str()),
            )
            .into_status()?;

            let results = self
                .state
                .like_service()
                .batch_counts(&refs)
                .await
                .into_status()?;

            let proto_results: Vec<social_v1::CountResponse> =
                results.into_iter().map(Into::into).collect();

            Ok(Response::new(social_v1::BatchCountsResponse {
                results: proto_results,
            }))
        }
        .await;

        Self::finish_request(METHOD, start, result)
    }

    // ── BatchStatuses ───────────────────────────────────────────────────

    async fn batch_statuses(
        &self,
        req: Request<social_v1::BatchStatusesRequest>,
    ) -> Result<Response<social_v1::BatchStatusesResponse>, Status> {
        const METHOD: &str = "social.v1.LikeService/BatchStatuses";
        let start = Instant::now();

        let result = async {
            let user = self.authenticate(req.metadata()).await?;
            self.check_user_read_limit(user.user_id).await?;

            let inner = req.into_inner();
            let refs = extract_content_refs(&inner.items)?;

            content::ensure_registered_content_types(
                self.state.config(),
                refs.iter().map(|(ct, _)| ct.as_str()),
            )
            .into_status()?;

            let results = self
                .state
                .like_service()
                .batch_statuses(user.user_id, &refs)
                .await
                .into_status()?;

            let proto_results: Vec<social_v1::StatusResponse> =
                results.into_iter().map(Into::into).collect();

            Ok(Response::new(social_v1::BatchStatusesResponse {
                results: proto_results,
            }))
        }
        .await;

        Self::finish_request(METHOD, start, result)
    }

    // ── GetUserLikes ────────────────────────────────────────────────────

    async fn get_user_likes(
        &self,
        req: Request<social_v1::GetUserLikesRequest>,
    ) -> Result<Response<social_v1::UserLikesResponse>, Status> {
        const METHOD: &str = "social.v1.LikeService/GetUserLikes";
        let start = Instant::now();

        let result = async {
            let user = self.authenticate(req.metadata()).await?;
            self.check_user_read_limit(user.user_id).await?;

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

            let items: Vec<social_v1::LikeItem> =
                result.items.into_iter().map(Into::into).collect();

            let pagination = social_v1::PaginationInfo {
                next_cursor: result.next_cursor,
                has_more: result.has_more,
            };

            Ok(Response::new(social_v1::UserLikesResponse {
                items,
                pagination: Some(pagination),
            }))
        }
        .await;

        Self::finish_request(METHOD, start, result)
    }

    // ── GetLeaderboard ──────────────────────────────────────────────────

    async fn get_leaderboard(
        &self,
        req: Request<social_v1::LeaderboardRequest>,
    ) -> Result<Response<social_v1::LeaderboardResponse>, Status> {
        const METHOD: &str = "social.v1.LikeService/GetLeaderboard";
        let start = Instant::now();

        let result = async {
            self.check_public_read_limit(&req).await?;

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

            Ok(Response::new(social_v1::LeaderboardResponse {
                items,
                window: result.window,
                content_type: result.content_type,
            }))
        }
        .await;

        Self::finish_request(METHOD, start, result)
    }

    // ── StreamLikes ─────────────────────────────────────────────────────

    type StreamLikesStream =
        Pin<Box<dyn Stream<Item = Result<social_v1::LikeEvent, Status>> + Send>>;

    async fn stream_likes(
        &self,
        req: Request<social_v1::StreamRequest>,
    ) -> Result<Response<Self::StreamLikesStream>, Status> {
        const METHOD: &str = "social.v1.LikeService/StreamLikes";
        let start = Instant::now();

        let result = async {
            self.check_public_read_limit(&req).await?;

            let inner = req.into_inner();
            self.validate_content_type(&inner.content_type)?;
            let _content_id = parse_uuid(&inner.content_id)?;

            let channel = format!("sse:{}:{}", inner.content_type, inner.content_id);
            let content_type = inner.content_type;
            let content_id_str = inner.content_id;

            let heartbeat_secs = self.state.config().sse_heartbeat_interval_secs;
            let shutdown_token = self.state.shutdown_token().clone();
            let pubsub_manager = self.state.pubsub_manager().clone();

            let rx = streaming::subscribe_like_events(&pubsub_manager, &channel)
                .await
                .map_err(|e| Status::internal(format!("Failed to subscribe to pubsub: {e}")))?;

            let stream = streaming::create_like_event_stream(rx, channel.clone(), heartbeat_secs, shutdown_token)
                .filter_map(move |item| {
                    let content_type = content_type.clone();
                    let content_id_str = content_id_str.clone();
                    let channel = channel.clone();

                    async move {
                        match item {
                            LikeStreamItem::Event(domain_event) => {
                                let proto = convert::to_proto_stream_event(
                                    domain_event,
                                    &content_type,
                                    &content_id_str,
                                );
                                Some(Ok(proto))
                            }
                            LikeStreamItem::Lagged(skipped) => {
                                tracing::debug!(channel = %channel, skipped, "gRPC stream client lagged, skipped messages");
                                None
                            }
                            LikeStreamItem::Closed => {
                                let proto = convert::to_proto_stream_event(
                                    shared::types::LikeEvent::Shutdown {
                                        timestamp: chrono::Utc::now(),
                                    },
                                    &content_type,
                                    &content_id_str,
                                );
                                Some(Ok(proto))
                            }
                        }
                    }
                });

            Ok(Response::new(Box::pin(stream) as Self::StreamLikesStream))
        }
        .await;

        Self::finish_request(METHOD, start, result)
    }
}
