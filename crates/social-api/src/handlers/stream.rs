use axum::{
    extract::{Query, State},
    response::{
        sse::{Event, Sse},
        IntoResponse,
    },
};
use futures::stream::Stream;
use futures::StreamExt;
use serde::Deserialize;
use std::convert::Infallible;
use std::time::Duration;

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    pub content_type: String,
    pub content_id: String,
}

/// GET /v1/likes/stream?content_type=post&content_id=UUID
/// SSE endpoint for real-time like/unlike events.
pub async fn like_stream(
    State(state): State<AppState>,
    Query(params): Query<StreamParams>,
) -> impl IntoResponse {
    let channel = format!("sse:{}:{}", params.content_type, params.content_id);
    let heartbeat_secs = state.config().sse_heartbeat_interval_secs;
    let redis_url = state.config().redis_url.clone();

    let stream = create_sse_stream(redis_url, channel, heartbeat_secs);

    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(heartbeat_secs))
            .text("heartbeat"),
    )
}

/// Creates an SSE stream that subscribes to a Redis Pub/Sub channel.
/// Uses a dedicated Redis connection (not from pool) for the subscription.
fn create_sse_stream(
    redis_url: String,
    channel: String,
    heartbeat_secs: u64,
) -> impl Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
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
                msg = msg_stream.next() => {
                    match msg {
                        Some(msg) => {
                            if let Ok(payload) = msg.get_payload::<String>() {
                                yield Ok(Event::default().data(payload));
                            }
                        }
                        None => {
                            // Channel closed
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
