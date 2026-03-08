use axum::{
    extract::{Query, State},
    response::{
        IntoResponse,
        sse::{Event, Sse},
    },
};
use futures::stream::Stream;
use serde::Deserialize;
use shared::errors::AppError;
use std::convert::Infallible;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::ApiErrorResponse;
use crate::content;
use crate::realtime::{self as streaming, LikeStreamItem};
use crate::services::pubsub_manager::PubSubManager;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    pub content_type: String,
    pub content_id: String,
}

/// GET /v1/likes/stream?content_type=post&content_id=UUID
/// SSE endpoint for real-time like/unlike events.
#[utoipa::path(
    get,
    path = "/v1/likes/stream",
    params(
        ("content_type" = String, Query, description = "Content type to subscribe to", example = "post"),
        ("content_id" = String, Query, description = "Content UUID to subscribe to", example = "731b0395-4888-4822-b516-05b4b7bf2089"),
    ),
    responses(
        (status = 200, description = "SSE stream opened. Events: like, unlike, heartbeat, shutdown", content_type = "text/event-stream"),
        (status = 400, description = "Invalid content_id (not a UUID) or unknown content_type"),
    ),
    tag = "Stream"
)]
pub async fn like_stream(
    State(state): State<AppState>,
    Query(params): Query<StreamParams>,
) -> Result<impl IntoResponse, ApiErrorResponse> {
    content::ensure_registered_content_type(state.config(), &params.content_type)?;

    // Validate content_id is a valid UUID
    let _content_id: Uuid = params.content_id.parse().map_err(|_| {
        ApiErrorResponse::from(AppError::InvalidContentId(params.content_id.clone()))
    })?;

    let channel = format!("sse:{}:{}", params.content_type, params.content_id);
    let heartbeat_secs = state.config().sse_heartbeat_interval_secs;
    let shutdown_token = state.shutdown_token().clone();
    let pubsub_manager = state.pubsub_manager().clone();

    let stream = create_sse_stream(pubsub_manager, channel, heartbeat_secs, shutdown_token);

    // No KeepAlive — our select! loop sends spec-compliant JSON heartbeats.
    Ok(Sse::new(stream))
}

/// Creates an SSE stream that subscribes via the shared PubSubManager.
/// Uses a single Redis connection per channel (shared across all SSE clients
/// on the same channel) instead of one Redis connection per client.
fn create_sse_stream(
    pubsub_manager: PubSubManager,
    channel: String,
    heartbeat_secs: u64,
    shutdown_token: CancellationToken,
) -> impl Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
        // Track active SSE connection inside the stream to avoid gauge
        // leaks if the client disconnects before the stream is polled.
        metrics::gauge!("social_api_sse_connections_active").increment(1.0);
        struct SseGuard;
        impl Drop for SseGuard {
            fn drop(&mut self) {
                metrics::gauge!("social_api_sse_connections_active").decrement(1.0);
            }
        }
        let _guard = SseGuard;

        // Subscribe via the shared PubSubManager (one Redis conn per channel)
        let rx = match streaming::subscribe_like_events(&pubsub_manager, &channel).await {
            Ok(rx) => rx,
            Err(e) => {
                tracing::warn!(error = %e, channel = %channel, "Failed to subscribe via PubSubManager");
                yield Ok(Event::default().event("error").data(r#"{"error":"Redis unavailable"}"#));
                return;
            }
        };

        tracing::debug!(channel = %channel, "SSE client subscribed via shared PubSub");
        let mut events = Box::pin(streaming::create_like_event_stream(
            rx,
            channel.clone(),
            heartbeat_secs,
            shutdown_token,
        ));

        while let Some(item) = futures::StreamExt::next(&mut events).await {
            match item {
                LikeStreamItem::Event(event) => match serde_json::to_string(&event) {
                    Ok(payload) => yield Ok(Event::default().data(payload)),
                    Err(error) => {
                        tracing::warn!(error = %error, channel = %channel, "Failed to serialize LikeEvent for SSE");
                    }
                },
                LikeStreamItem::Lagged(skipped) => {
                    tracing::debug!(channel = %channel, skipped, "SSE client lagged, skipped messages");
                }
                LikeStreamItem::Closed => {
                    tracing::debug!(channel = %channel, "SSE stream closed because the Pub/Sub bridge ended");
                    break;
                }
            }
        }
    }
}
