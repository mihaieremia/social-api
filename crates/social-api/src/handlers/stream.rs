use axum::{
    extract::{Query, State},
    response::{
        IntoResponse,
        sse::{Event, Sse},
    },
};
use futures::StreamExt;
use futures::stream::Stream;
use serde::Deserialize;
use shared::errors::AppError;
use std::convert::Infallible;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::errors::ApiErrorResponse;
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
    // Validate content_type against registered types
    if !state.config().is_valid_content_type(&params.content_type) {
        return Err(AppError::ContentTypeUnknown(params.content_type.clone()).into());
    }

    // Validate content_id is a valid UUID
    let _content_id: Uuid = params.content_id.parse().map_err(|_| {
        ApiErrorResponse::from(AppError::InvalidContentId(params.content_id.clone()))
    })?;

    let channel = format!("sse:{}:{}", params.content_type, params.content_id);
    let heartbeat_secs = state.config().sse_heartbeat_interval_secs;
    let redis_url = state.config().redis_url.clone();
    let shutdown_token = state.shutdown_token().clone();

    let stream = create_sse_stream(redis_url, channel, heartbeat_secs, shutdown_token);

    // No KeepAlive — our select! loop sends spec-compliant JSON heartbeats.
    Ok(Sse::new(stream))
}

/// Creates an SSE stream that subscribes to a Redis Pub/Sub channel.
/// Uses a dedicated Redis connection (not from pool) for the subscription.
/// Monitors the CancellationToken for graceful shutdown.
fn create_sse_stream(
    redis_url: String,
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

        // Create a dedicated Redis connection for Pub/Sub
        let client = match redis::Client::open(redis_url.as_str()) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to create Redis client for SSE");
                yield Ok(Event::default().event("error").data(r#"{"error":"Redis unavailable"}"#));
                return;
            }
        };

        let mut pubsub = match client.get_async_pubsub().await {
            Ok(ps) => ps,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to get Pub/Sub connection for SSE");
                yield Ok(Event::default().event("error").data(r#"{"error":"Redis unavailable"}"#));
                return;
            }
        };

        if let Err(e) = pubsub.subscribe(&channel).await {
            tracing::warn!(error = %e, channel = %channel, "Failed to subscribe to SSE channel");
            yield Ok(Event::default().event("error").data(r#"{"error":"Subscribe failed"}"#));
            return;
        }

        tracing::debug!(channel = %channel, "SSE client subscribed");

        let mut heartbeat = tokio::time::interval(Duration::from_secs(heartbeat_secs));
        let mut msg_stream = pubsub.on_message();

        loop {
            tokio::select! {
                // Shutdown signal takes priority
                _ = shutdown_token.cancelled() => {
                    let ts = chrono::Utc::now().to_rfc3339();
                    yield Ok(Event::default().data(
                        format!(r#"{{"event":"shutdown","timestamp":"{ts}"}}"#)
                    ));
                    tracing::debug!(channel = %channel, "SSE stream closed by shutdown");
                    break;
                }
                msg = msg_stream.next() => {
                    match msg {
                        Some(msg) => {
                            if let Ok(payload) = msg.get_payload::<String>() {
                                yield Ok(Event::default().data(payload));
                            }
                        }
                        None => {
                            // Redis channel closed unexpectedly
                            let ts = chrono::Utc::now().to_rfc3339();
                            yield Ok(Event::default().data(
                                format!(r#"{{"event":"shutdown","timestamp":"{ts}"}}"#)
                            ));
                            break;
                        }
                    }
                }
                _ = heartbeat.tick() => {
                    let ts = chrono::Utc::now().to_rfc3339();
                    yield Ok(Event::default().data(
                        format!(r#"{{"event":"heartbeat","timestamp":"{ts}"}}"#)
                    ));
                }
            }
        }
    }
}
