use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use futures::StreamExt;
use tokio::sync::broadcast;

/// Manages shared Redis Pub/Sub subscriptions with in-process fan-out.
///
/// Instead of opening one Redis connection per SSE client, this maintains
/// a single Redis Pub/Sub connection per channel and uses `tokio::sync::broadcast`
/// to fan out messages to all subscribers in-process.
///
/// Hardened with:
/// - **DashMap** for lock-free concurrent channel access (no Mutex contention)
/// - **Auto-reconnect** with exponential backoff on Redis failures
/// - **Configurable broadcast buffer** via `SSE_BROADCAST_CAPACITY` env var
#[derive(Clone)]
pub struct PubSubManager {
    inner: Arc<PubSubManagerInner>,
}

struct PubSubManagerInner {
    redis_url: String,
    broadcast_capacity: usize,
    /// Lock-free concurrent map: channel name -> broadcast sender.
    channels: DashMap<String, broadcast::Sender<String>>,
}

/// Max reconnect attempts before the bridge gives up.
const MAX_RECONNECT_ATTEMPTS: u32 = 10;

/// Initial backoff delay for reconnection (doubles each attempt, caps at 30s).
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How often to check if all receivers have dropped (idle check).
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(2);

impl PubSubManager {
    pub fn new(redis_url: String, broadcast_capacity: usize) -> Self {
        Self {
            inner: Arc::new(PubSubManagerInner {
                redis_url,
                broadcast_capacity,
                channels: DashMap::new(),
            }),
        }
    }

    /// Subscribe to a Redis channel. Returns a broadcast receiver.
    ///
    /// If this is the first subscriber for the channel, spawns a background
    /// bridge task that maintains the Redis Pub/Sub connection with auto-reconnect.
    ///
    /// Uses DashMap for lock-free access — no serialization of concurrent subscribes.
    pub async fn subscribe(&self, channel: &str) -> Result<broadcast::Receiver<String>, String> {
        // Fast path: channel already exists with active subscribers
        if let Some(entry) = self.inner.channels.get(channel) {
            let tx = entry.value();
            // Reuse if there are still receivers (bridge is alive)
            if tx.receiver_count() > 0 {
                return Ok(tx.subscribe());
            }
            // Stale entry — drop ref before removing to avoid DashMap deadlock
            drop(entry);
            self.inner.channels.remove(channel);
        }

        // Create new broadcast channel with configurable capacity
        let (tx, rx) = broadcast::channel(self.inner.broadcast_capacity);

        // Insert into DashMap — if another thread raced us, use theirs
        let tx = match self.inner.channels.entry(channel.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(existing) => {
                // Another thread won the race — use their sender
                let existing_tx = existing.get().clone();
                return Ok(existing_tx.subscribe());
            }
            dashmap::mapref::entry::Entry::Vacant(vacant) => {
                vacant.insert(tx.clone());
                tx
            }
        };

        // Spawn bridge task with auto-reconnect
        let redis_url = self.inner.redis_url.clone();
        let channel_name = channel.to_string();
        let channels_ref = self.inner.channels.clone();

        tokio::spawn(async move {
            run_bridge_with_reconnect(&redis_url, &channel_name, &tx).await;

            // Clean up: only remove if no receivers remain (avoid race with new subscriber)
            if let Some(entry) = channels_ref.get(&channel_name)
                && entry.value().receiver_count() == 0
            {
                drop(entry);
                channels_ref.remove(&channel_name);
                tracing::debug!(channel = %channel_name, "Removed idle Pub/Sub channel");
            }
        });

        Ok(rx)
    }

    /// Returns the number of active channels (for metrics/debugging).
    #[allow(dead_code)]
    pub fn active_channels(&self) -> usize {
        self.inner.channels.len()
    }
}

/// Runs the pub/sub bridge with automatic reconnection on failure.
/// Retries with exponential backoff up to MAX_RECONNECT_ATTEMPTS.
/// Exits permanently when all receivers are dropped or retries exhausted.
async fn run_bridge_with_reconnect(redis_url: &str, channel: &str, tx: &broadcast::Sender<String>) {
    let mut attempt: u32 = 0;

    loop {
        // Bail if no subscribers remain
        if tx.receiver_count() == 0 {
            tracing::debug!(channel = %channel, "No subscribers, bridge exiting");
            return;
        }

        match run_single_connection(redis_url, channel, tx).await {
            BridgeExit::NoReceivers => {
                tracing::debug!(channel = %channel, "All receivers dropped, bridge exiting");
                return;
            }
            BridgeExit::RedisError(e) => {
                attempt += 1;
                if attempt > MAX_RECONNECT_ATTEMPTS {
                    tracing::error!(
                        channel = %channel,
                        attempts = MAX_RECONNECT_ATTEMPTS,
                        "Pub/Sub bridge exhausted reconnect attempts, giving up"
                    );
                    return;
                }

                let backoff = std::cmp::min(
                    INITIAL_BACKOFF * 2u32.saturating_pow(attempt - 1),
                    MAX_BACKOFF,
                );

                tracing::warn!(
                    channel = %channel,
                    error = %e,
                    attempt = attempt,
                    backoff_ms = backoff.as_millis(),
                    "Pub/Sub bridge disconnected, reconnecting"
                );

                tokio::time::sleep(backoff).await;

                // Check again before reconnecting
                if tx.receiver_count() == 0 {
                    tracing::debug!(channel = %channel, "No subscribers during backoff, bridge exiting");
                    return;
                }
            }
        }
    }
}

/// Result of a single bridge connection lifecycle.
enum BridgeExit {
    /// All broadcast receivers dropped — no one is listening.
    NoReceivers,
    /// Redis connection failed or stream ended unexpectedly.
    RedisError(String),
}

/// Maintains a single Redis Pub/Sub subscription and forwards messages
/// to the broadcast sender. Returns the reason it stopped.
async fn run_single_connection(
    redis_url: &str,
    channel: &str,
    tx: &broadcast::Sender<String>,
) -> BridgeExit {
    // Connect to Redis
    let client = match redis::Client::open(redis_url) {
        Ok(c) => c,
        Err(e) => return BridgeExit::RedisError(format!("Client open: {e}")),
    };

    let mut pubsub = match client.get_async_pubsub().await {
        Ok(ps) => ps,
        Err(e) => return BridgeExit::RedisError(format!("Pub/Sub connect: {e}")),
    };

    if let Err(e) = pubsub.subscribe(channel).await {
        return BridgeExit::RedisError(format!("Subscribe: {e}"));
    }

    tracing::debug!(channel = %channel, "Pub/Sub bridge connected");

    // Reset reconnect counter on successful connection (caller tracks this)
    let exit_reason;

    {
        let mut msg_stream = pubsub.on_message();

        exit_reason = loop {
            if tx.receiver_count() == 0 {
                break BridgeExit::NoReceivers;
            }

            match tokio::time::timeout(IDLE_CHECK_INTERVAL, msg_stream.next()).await {
                Ok(Some(msg)) => {
                    if let Ok(payload) = msg.get_payload::<String>()
                        && tx.send(payload).is_err()
                    {
                        break BridgeExit::NoReceivers;
                    }
                }
                Ok(None) => {
                    break BridgeExit::RedisError("Stream ended unexpectedly".to_string());
                }
                Err(_) => {
                    // Timeout — loop to check receiver_count
                    continue;
                }
            }
        };
    } // msg_stream dropped, releasing borrow on pubsub

    // Best-effort unsubscribe (don't block on failure)
    let _ = tokio::time::timeout(Duration::from_secs(2), pubsub.unsubscribe(channel)).await;

    exit_reason
}

#[cfg(test)]
mod tests {
    use super::*;
    use testcontainers::runners::AsyncRunner;

    /// Start a Redis testcontainer and return a PubSubManager + the URL + the container.
    /// The container must be kept alive for the duration of the test.
    async fn make_manager() -> (
        PubSubManager,
        String,
        testcontainers::ContainerAsync<testcontainers_modules::redis::Redis>,
    ) {
        let redis = testcontainers_modules::redis::Redis::default()
            .start()
            .await
            .expect("redis container");
        let port = redis.get_host_port_ipv4(6379).await.unwrap();
        let redis_url = format!("redis://127.0.0.1:{port}");
        let mgr = PubSubManager::new(redis_url.clone(), 16);
        (mgr, redis_url, redis)
    }

    #[tokio::test]
    async fn test_subscribe_and_receive_message() {
        let (mgr, redis_url, _redis) = make_manager().await;
        let channel = "test:pubsub:recv";

        let mut rx = mgr.subscribe(channel).await.expect("subscribe");

        // Give the bridge task time to connect to Redis
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Publish a message via a separate redis::Client
        let client = redis::Client::open(redis_url.as_str()).expect("client open");
        let mut conn = client
            .get_multiplexed_async_connection()
            .await
            .expect("connection");
        let _: i64 = redis::cmd("PUBLISH")
            .arg(channel)
            .arg("hello-pubsub")
            .query_async(&mut conn)
            .await
            .expect("publish");

        // Wait up to 2 seconds for the message to arrive
        let received = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for message")
            .expect("recv error");

        assert_eq!(received, "hello-pubsub");
    }

    #[tokio::test]
    async fn test_subscribe_same_channel_reuses_bridge() {
        // This test verifies the DashMap channel-reuse logic.
        // We use a real Redis container so the bridge can connect and stay alive
        // long enough for the second subscribe to hit the fast path.
        let (mgr, _redis_url, _redis) = make_manager().await;
        let channel = "test:pubsub:reuse";

        let _rx1 = mgr.subscribe(channel).await.expect("subscribe 1");
        // Give the bridge task a moment to connect before the second subscribe
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _rx2 = mgr.subscribe(channel).await.expect("subscribe 2");

        // Both subscribers share a single bridge — only one channel entry.
        assert_eq!(mgr.active_channels(), 1);
    }

    #[tokio::test]
    async fn test_subscribe_different_channels_create_separate_bridges() {
        // No real Redis needed: subscribe() always succeeds (bridge reconnects in bg).
        let mgr = PubSubManager::new("redis://127.0.0.1:19998".to_string(), 16);

        let _rx1 = mgr.subscribe("test:pubsub:ch1").await.expect("subscribe ch1");
        let _rx2 = mgr.subscribe("test:pubsub:ch2").await.expect("subscribe ch2");

        assert_eq!(mgr.active_channels(), 2);
    }

    #[tokio::test]
    async fn test_subscribe_bad_redis_url_does_not_panic() {
        // A bad URL means the bridge background task will fail to connect,
        // but subscribe() itself must return Ok (bridge reconnects in the background).
        let mgr = PubSubManager::new("redis://127.0.0.1:19996".to_string(), 16);
        let result = mgr.subscribe("test:bad").await;
        assert!(result.is_ok(), "subscribe should not panic on bad URL");
    }

    #[tokio::test]
    async fn test_active_channels_starts_at_zero() {
        let mgr = PubSubManager::new("redis://127.0.0.1:19997".to_string(), 16);
        assert_eq!(mgr.active_channels(), 0);
    }

    #[tokio::test]
    async fn test_multiple_messages_delivered_in_order() {
        let (mgr, redis_url, _redis) = make_manager().await;
        let channel = "test:pubsub:ordered";

        let mut rx = mgr.subscribe(channel).await.expect("subscribe");

        // Give bridge time to connect
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let client = redis::Client::open(redis_url.as_str()).expect("client open");
        let mut conn = client
            .get_multiplexed_async_connection()
            .await
            .expect("connection");

        for i in 0u32..3 {
            let _: i64 = redis::cmd("PUBLISH")
                .arg(channel)
                .arg(format!("msg-{i}"))
                .query_async(&mut conn)
                .await
                .expect("publish");
        }

        let timeout = std::time::Duration::from_secs(3);
        for i in 0u32..3 {
            let msg = tokio::time::timeout(timeout, rx.recv())
                .await
                .expect("timed out")
                .expect("recv error");
            assert_eq!(msg, format!("msg-{i}"));
        }
    }
}
