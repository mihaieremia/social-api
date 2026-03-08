use std::time::Duration;

use futures::Stream;
use shared::types::LikeEvent;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::services::pubsub_manager::PubSubManager;

pub enum LikeStreamItem {
    Event(LikeEvent),
    Lagged(u64),
    Closed,
}

pub async fn subscribe_like_events(
    pubsub_manager: &PubSubManager,
    channel: &str,
) -> Result<broadcast::Receiver<String>, String> {
    pubsub_manager.subscribe(channel).await
}

pub fn create_like_event_stream(
    mut rx: broadcast::Receiver<String>,
    channel: String,
    heartbeat_secs: u64,
    shutdown_token: CancellationToken,
) -> impl Stream<Item = LikeStreamItem> {
    async_stream::stream! {
        let mut heartbeat = tokio::time::interval(Duration::from_secs(heartbeat_secs));

        loop {
            tokio::select! {
                _ = shutdown_token.cancelled() => {
                    yield LikeStreamItem::Event(LikeEvent::Shutdown {
                        timestamp: chrono::Utc::now(),
                    });
                    break;
                }
                _ = heartbeat.tick() => {
                    yield LikeStreamItem::Event(LikeEvent::Heartbeat {
                        timestamp: chrono::Utc::now(),
                    });
                }
                result = rx.recv() => {
                    match result {
                        Ok(payload) => match serde_json::from_str::<LikeEvent>(&payload) {
                            Ok(event) => yield LikeStreamItem::Event(event),
                            Err(error) => {
                                tracing::warn!(
                                    error = %error,
                                    channel = %channel,
                                    "Failed to deserialize LikeEvent from pubsub payload"
                                );
                            }
                        },
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            yield LikeStreamItem::Lagged(skipped);
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            yield LikeStreamItem::Closed;
                            break;
                        }
                    }
                }
            }
        }
    }
}
