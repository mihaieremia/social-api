use shared::types::LikeEvent;
use uuid::Uuid;

use crate::cache::CacheManager;

/// Publishes like/unlike events to Redis Pub/Sub for real-time SSE fan-out,
/// and records per-content-type operation metrics.
#[derive(Clone)]
pub(crate) struct LikeEventPublisher {
    cache: CacheManager,
}

impl LikeEventPublisher {
    pub(crate) fn new(cache: CacheManager) -> Self {
        Self { cache }
    }

    /// Publish a like event to the `sse:{type}:{id}` Redis Pub/Sub channel.
    pub(crate) async fn publish(&self, event: LikeEvent, content_type: &str, content_id: Uuid) {
        let channel = format!("sse:{content_type}:{content_id}");
        if let Ok(json) = serde_json::to_string(&event) {
            self.cache.publish(&channel, &json).await;
        }
    }

    /// Increment the `social_api_likes_total{content_type, operation}` counter.
    pub(crate) fn record_metric(operation: &'static str, content_type: &str) {
        metrics::counter!(
            "social_api_likes_total",
            "content_type" => content_type.to_string(),
            "operation" => operation,
        )
        .increment(1);
    }
}
