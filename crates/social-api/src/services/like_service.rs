use std::sync::Arc;

use chrono::{Duration, Utc};
use dashmap::DashMap;
use shared::errors::AppError;
use shared::types::*;
use tokio::sync::watch;
use uuid::Uuid;

use crate::cache::manager::CacheManager;
use crate::clients::circuit_breaker::CircuitBreaker;
use crate::clients::content_client::{ContentValidator, HttpContentValidator};
use crate::config::Config;
use crate::db::DbPools;
use crate::repositories::like_repository;

/// Core like service handling all like-related business logic.
pub struct LikeService {
    db: DbPools,
    cache: CacheManager,
    /// Trait object — swappable transport (HTTP or gRPC via INTERNAL_TRANSPORT).
    content_validator: Arc<dyn ContentValidator>,
    config: Config,
    content_breaker: Arc<CircuitBreaker>,
    /// In-progress cache fetches, keyed by cache key.
    /// Used for stampede coalescing — see `get_count_inner`.
    pending_fetches: DashMap<String, watch::Sender<Option<i64>>>,
}

impl LikeService {
    pub fn new(
        db: DbPools,
        cache: CacheManager,
        http_client: reqwest::Client,
        config: Config,
        content_breaker: Arc<CircuitBreaker>,
    ) -> Self {
        let content_validator = Arc::new(HttpContentValidator::new(
            http_client,
            cache.clone(),
            config.clone(),
        ));
        Self {
            db,
            cache,
            content_validator,
            config,
            content_breaker,
            pending_fetches: DashMap::new(),
        }
    }

    /// Constructor accepting a pre-built ContentValidator trait object.
    /// Used by integration tests and anywhere a custom validator is needed.
    #[allow(dead_code)]
    pub fn new_with_validator(
        db: DbPools,
        cache: CacheManager,
        content_validator: Arc<dyn ContentValidator>,
        config: Config,
        content_breaker: Arc<CircuitBreaker>,
    ) -> Self {
        Self {
            db,
            cache,
            content_validator,
            config,
            content_breaker,
            pending_fetches: DashMap::new(),
        }
    }

    /// Like content. Idempotent — duplicate requests return success.
    ///
    /// **Pre-condition:** `content_type` must be validated by the caller (handler/extractor).
    pub async fn like(
        &self,
        user_id: Uuid,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeActionResponse, AppError> {
        // Validate content exists BEFORE inserting.
        // This prevents a race where a concurrent duplicate request could observe
        // an unvalidated row and return success before this request rolls it back.
        self.validate_content(content_type, content_id).await?;

        // Now insert — content is known-valid. Duplicate detection is safe because
        // any existing row was validated on its original insert.
        let result =
            like_repository::insert_like(&self.db.writer, user_id, content_type, content_id)
                .await
                .map_err(Self::db_err)?;

        match result {
            like_repository::InsertLikeResult::Inserted { row, count } => {
                self.write_count_to_cache(content_type, content_id, count)
                    .await;
                self.publish_like_event(
                    LikeEvent::Liked {
                        user_id,
                        count,
                        timestamp: row.created_at,
                    },
                    content_type,
                    content_id,
                )
                .await;
                Self::record_like_metric("like", content_type);
                Ok(LikeActionResponse {
                    liked: true,
                    already_existed: Some(false),
                    was_liked: None,
                    count,
                    liked_at: Some(row.created_at),
                })
            }
            like_repository::InsertLikeResult::AlreadyExisted { row, count } => {
                self.write_count_to_cache(content_type, content_id, count)
                    .await;
                Ok(LikeActionResponse {
                    liked: true,
                    already_existed: Some(true),
                    was_liked: None,
                    count,
                    liked_at: Some(row.created_at),
                })
            }
            like_repository::InsertLikeResult::ConcurrentlyRemoved { count } => {
                // Extremely rare: like was concurrently removed. Return stable
                // idempotent response without fabricated timestamp.
                Ok(LikeActionResponse {
                    liked: true,
                    already_existed: Some(true),
                    was_liked: None,
                    count,
                    liked_at: None,
                })
            }
        }
    }

    /// Unlike content. Idempotent — unliking content not liked returns success.
    ///
    /// **Pre-condition:** `content_type` must be validated by the caller (handler/extractor).
    pub async fn unlike(
        &self,
        user_id: Uuid,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeActionResponse, AppError> {
        // Validate content exists — spec requires same validation chain as like.
        self.validate_content(content_type, content_id).await?;

        let result =
            like_repository::delete_like(&self.db.writer, user_id, content_type, content_id)
                .await
                .map_err(Self::db_err)?;

        // Use the authoritative count from the write transaction — same fix as like().
        let count = result.count;
        self.write_count_to_cache(content_type, content_id, count)
            .await;

        // Publish SSE event
        if result.was_liked {
            self.publish_like_event(
                LikeEvent::Unliked {
                    user_id,
                    count,
                    timestamp: Utc::now(),
                },
                content_type,
                content_id,
            )
            .await;
            Self::record_like_metric("unlike", content_type);
        }

        Ok(LikeActionResponse {
            liked: false,
            already_existed: None,
            was_liked: Some(result.was_liked),
            count,
            liked_at: None,
        })
    }

    /// Get like count with cache-first strategy and stampede protection.
    ///
    /// **Pre-condition:** `content_type` must be validated by the caller (handler/extractor).
    pub async fn get_count(
        &self,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeCountResponse, AppError> {
        let count = self.get_count_inner(content_type, content_id).await?;

        Ok(LikeCountResponse {
            content_type: content_type.to_string(),
            content_id,
            count,
        })
    }

    /// Internal count getter with cache-first strategy and stampede protection.
    ///
    /// Uses `DashMap::entry()` for atomic leader election — exactly one task
    /// fetches from DB while concurrent tasks subscribe to the same `watch` channel.
    async fn get_count_inner(&self, content_type: &str, content_id: Uuid) -> Result<i64, AppError> {
        let cache_key = count_cache_key(content_type, content_id);

        // Fast path: cache hit
        if let Some(cached) = self.cache.get(&cache_key).await
            && let Ok(count) = cached.parse::<i64>()
        {
            return Ok(count);
        }

        // Atomic leader election via DashMap::entry().
        // Occupied → another task is already fetching; subscribe as follower.
        // Vacant   → we are the leader; register a watch channel and fetch from DB.
        enum FetchRole {
            Leader(watch::Sender<Option<i64>>),
            Follower(watch::Receiver<Option<i64>>),
        }

        let role = match self.pending_fetches.entry(cache_key.clone()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => {
                let rx = entry.get().subscribe();
                FetchRole::Follower(rx)
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let (tx, _rx) = watch::channel(None::<i64>);
                entry.insert(tx.clone());
                FetchRole::Leader(tx)
            }
        };

        match role {
            FetchRole::Follower(mut rx) => {
                // Await the leader's result — no DB query needed.
                rx.changed().await.ok();
                if let Some(count) = *rx.borrow() {
                    return Ok(count);
                }
                // Leader failed — fall through to a direct DB read.
                like_repository::get_count(&self.db.reader, content_type, content_id)
                    .await
                    .map_err(Self::db_err)
            }
            FetchRole::Leader(tx) => {
                let result =
                    like_repository::get_count(&self.db.reader, content_type, content_id).await;

                // Always clean up the DashMap entry — on success AND failure.
                match result {
                    Ok(count) => {
                        let _ = tx.send(Some(count));
                        self.pending_fetches.remove(&cache_key);
                        self.write_count_to_cache(content_type, content_id, count)
                            .await;
                        Ok(count)
                    }
                    Err(e) => {
                        // Drop sender first so followers see the close,
                        // then remove the entry.
                        self.pending_fetches.remove(&cache_key);
                        Err(Self::db_err(e))
                    }
                }
            }
        }
    }

    /// Get like status for a user on a content item.
    ///
    /// **Pre-condition:** `content_type` must be validated by the caller (handler/extractor).
    pub async fn get_status(
        &self,
        user_id: Uuid,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeStatusResponse, AppError> {
        let liked_at =
            like_repository::get_like_status(&self.db.reader, user_id, content_type, content_id)
                .await
                .map_err(Self::db_err)?;

        Ok(LikeStatusResponse {
            liked: liked_at.is_some(),
            liked_at,
        })
    }

    /// Get user's liked items with cursor-based pagination.
    pub async fn get_user_likes(
        &self,
        user_id: Uuid,
        content_type_filter: Option<&str>,
        cursor: Option<&str>,
        limit: i64,
    ) -> Result<PaginatedResponse<UserLikeItem>, AppError> {
        let limit = limit.clamp(1, 100);

        // Decode cursor if provided
        let (cursor_ts, cursor_id) = if let Some(cursor_str) = cursor {
            let c = shared::cursor::Cursor::decode(cursor_str)
                .map_err(|_| AppError::InvalidCursor("Malformed cursor".to_string()))?;
            (Some(c.timestamp), Some(c.id))
        } else {
            (None, None)
        };

        let rows = like_repository::get_user_likes(
            &self.db.reader,
            user_id,
            content_type_filter,
            cursor_ts,
            cursor_id,
            limit,
        )
        .await
        .map_err(Self::db_err)?;

        let has_more = rows.len() as i64 > limit;
        let take_n = (limit as usize).min(rows.len());
        let mut items = Vec::with_capacity(take_n);
        for r in rows.iter().take(take_n) {
            items.push(UserLikeItem {
                content_type: r.content_type.clone(),
                content_id: r.content_id,
                liked_at: r.created_at,
            });
        }

        let next_cursor = if has_more {
            items.last().map(|last| {
                let last_row = &rows[items.len() - 1];
                shared::cursor::Cursor::new(last.liked_at, last_row.id).encode()
            })
        } else {
            None
        };

        Ok(PaginatedResponse {
            items,
            next_cursor,
            has_more,
        })
    }

    /// Batch get like counts.
    ///
    /// 1. MGET all keys from Redis.
    /// 2. Deduplicate cache misses — multiple request items for the same key share one fetch.
    /// 3. Fetch all unique misses in a single batched SQL query.
    /// 4. Pipeline-SETEX all fetched values back into Redis.
    pub async fn batch_counts(
        &self,
        items: &[(String, Uuid)],
    ) -> Result<Vec<BatchCountResult>, AppError> {
        if items.len() > 100 {
            return Err(AppError::BatchTooLarge {
                size: items.len(),
                max: 100,
            });
        }

        // Try Redis MGET first
        let mut cache_keys = Vec::with_capacity(items.len());
        for (ct, cid) in items {
            cache_keys.push(count_cache_key(ct, *cid));
        }

        let cached_values = self.cache.mget(&cache_keys).await;

        let mut results = Vec::with_capacity(items.len());
        // Dedup: map unique cache_key → (first missing position, list of all positions)
        let mut missing_map: std::collections::HashMap<String, Vec<usize>> =
            std::collections::HashMap::new();

        for (i, (ct, cid)) in items.iter().enumerate() {
            if let Some(Some(val)) = cached_values.get(i)
                && let Ok(count) = val.parse::<i64>()
            {
                results.push(BatchCountResult {
                    content_type: ct.clone(),
                    content_id: *cid,
                    count,
                });
                continue;
            }
            // Track position for filling later
            missing_map
                .entry(cache_keys[i].clone())
                .or_default()
                .push(i);
            results.push(BatchCountResult {
                content_type: ct.clone(),
                content_id: *cid,
                count: 0,
            });
        }

        if !missing_map.is_empty() {
            // Single-flight coalescing: for each unique missing key, atomically
            // elect a leader via DashMap::entry(). Leaders fetch from DB; followers
            // subscribe and await the result.
            let mut to_fetch: Vec<(String, Uuid)> = Vec::new(); // keys this request must fetch
            let mut leader_keys: Vec<String> = Vec::new(); // cache keys we are leader for
            let mut follower_rxs: Vec<(String, watch::Receiver<Option<i64>>)> = Vec::new();

            for cache_key in missing_map.keys() {
                match self.pending_fetches.entry(cache_key.clone()) {
                    dashmap::mapref::entry::Entry::Occupied(entry) => {
                        let rx = entry.get().subscribe();
                        follower_rxs.push((cache_key.clone(), rx));
                    }
                    dashmap::mapref::entry::Entry::Vacant(entry) => {
                        let (tx, _rx) = watch::channel(None::<i64>);
                        entry.insert(tx);
                        let first_pos = missing_map[cache_key][0];
                        to_fetch.push(items[first_pos].clone());
                        leader_keys.push(cache_key.clone());
                    }
                }
            }

            // Leader path: batch-fetch all keys we own from DB
            if !to_fetch.is_empty() {
                let db_result = like_repository::batch_get_counts(&self.db.reader, &to_fetch).await;

                match db_result {
                    Ok(db_rows) => {
                        let count_map: std::collections::HashMap<(String, Uuid), i64> = db_rows
                            .into_iter()
                            .map(|(ct, cid, count)| ((ct, cid), count))
                            .collect();

                        let ttl = self.config.cache_ttl_like_counts_secs;
                        let mut cache_entries: Vec<(String, String)> =
                            Vec::with_capacity(leader_keys.len());

                        for cache_key in &leader_keys {
                            let positions = &missing_map[cache_key];
                            let first_pos = positions[0];
                            let key = &items[first_pos];
                            let count = count_map.get(key).copied().unwrap_or(0);

                            // Fill result positions
                            for &pos in positions {
                                results[pos].count = count;
                            }

                            // Notify followers via watch channel
                            if let Some((_, tx)) = self.pending_fetches.remove(cache_key) {
                                let _ = tx.send(Some(count));
                            }

                            cache_entries.push((cache_key.clone(), count.to_string()));
                        }

                        // Pipeline SETEX all fetched values in one Redis round-trip
                        let set_entries: Vec<(&str, &str, u64)> = cache_entries
                            .iter()
                            .map(|(k, v)| (k.as_str(), v.as_str(), ttl))
                            .collect();
                        self.cache.set_many(&set_entries).await;
                    }
                    Err(e) => {
                        // Clean up all leader entries so followers see the sender drop
                        for cache_key in &leader_keys {
                            self.pending_fetches.remove(cache_key);
                        }
                        return Err(Self::db_err(e));
                    }
                }
            }

            // Follower path: await results from the leader's fetch
            for (cache_key, mut rx) in follower_rxs {
                let changed = rx.changed().await.is_ok();
                let count = if changed {
                    rx.borrow().as_ref().copied()
                } else {
                    None
                };

                let count = if let Some(count) = count {
                    count
                } else if let Some(positions) = missing_map.get(&cache_key) {
                    // Leader failed or dropped before publishing. Mirror
                    // get_count_inner() by falling back to a direct DB read.
                    let first_pos = positions[0];
                    let (content_type, content_id) = &items[first_pos];
                    let count =
                        like_repository::get_count(&self.db.reader, content_type, *content_id)
                            .await
                            .map_err(Self::db_err)?;
                    self.write_count_to_cache(content_type, *content_id, count)
                        .await;
                    count
                } else {
                    continue;
                };

                if let Some(positions) = missing_map.get(&cache_key) {
                    for &pos in positions {
                        results[pos].count = count;
                    }
                }
            }
        }

        Ok(results)
    }

    /// Batch get like statuses for a user.
    pub async fn batch_statuses(
        &self,
        user_id: Uuid,
        items: &[(String, Uuid)],
    ) -> Result<Vec<BatchStatusResult>, AppError> {
        if items.len() > 100 {
            return Err(AppError::BatchTooLarge {
                size: items.len(),
                max: 100,
            });
        }

        let rows = like_repository::batch_get_statuses(&self.db.reader, user_id, items)
            .await
            .map_err(Self::db_err)?;

        // Build result map from DB results
        let mut result_map: std::collections::HashMap<
            (String, Uuid),
            Option<chrono::DateTime<Utc>>,
        > = rows
            .into_iter()
            .map(|(ct, cid, ts)| ((ct, cid), ts))
            .collect();

        // Ensure all requested items appear in results
        let mut results = Vec::with_capacity(items.len());
        for (ct, cid) in items {
            let liked_at = result_map.remove(&(ct.clone(), *cid)).flatten();
            results.push(BatchStatusResult {
                content_type: ct.clone(),
                content_id: *cid,
                liked: liked_at.is_some(),
                liked_at,
            });
        }

        Ok(results)
    }

    /// Get top liked content leaderboard.
    ///
    /// Tries Redis sorted set `lb:{window}` first (populated by the background
    /// refresh task). Falls back to a direct DB query on cache miss or error.
    pub async fn get_leaderboard(
        &self,
        content_type: Option<&str>,
        window: TimeWindow,
        limit: i64,
    ) -> Result<TopLikedResponse, AppError> {
        let limit = limit.clamp(1, 50);

        // Try Redis cache first (only when no content_type filter, since the
        // background task stores the global leaderboard per window).
        if content_type.is_none() {
            let key = format!("lb:{}", window.as_str());
            let cached = self
                .cache
                .zrevrange_with_scores(&key, 0, (limit - 1) as isize)
                .await;

            if !cached.is_empty() {
                let mut items = Vec::with_capacity(cached.len());
                for (member, score) in &cached {
                    if let Some(item) = parse_leaderboard_member(member, *score) {
                        items.push(item);
                    }
                }

                return Ok(TopLikedResponse {
                    window: window.as_str().to_string(),
                    content_type: None,
                    items,
                });
            }
        }

        // Cache miss or content_type filter — fall back to DB.
        let since = window
            .duration_secs()
            .map(|secs| Utc::now() - Duration::seconds(secs));

        let rows = like_repository::get_leaderboard(&self.db.reader, content_type, since, limit)
            .await
            .map_err(Self::db_err)?;

        let mut items = Vec::with_capacity(rows.len());
        for (ct, cid, count) in rows {
            items.push(TopLikedItem {
                content_type: ct,
                content_id: cid,
                count,
            });
        }

        Ok(TopLikedResponse {
            window: window.as_str().to_string(),
            content_type: content_type.map(|s| s.to_string()),
            items,
        })
    }

    /// Write an authoritative count to the cache with the configured TTL.
    ///
    /// Used by `like()` and `unlike()` after the DB transaction returns the
    /// new total_count. This is a write-through pattern — the cache always
    /// reflects the value the DB just told us.
    async fn write_count_to_cache(&self, content_type: &str, content_id: Uuid, count: i64) {
        let cache_key = count_cache_key(content_type, content_id);
        self.cache
            .set(
                &cache_key,
                &count.to_string(),
                self.config.cache_ttl_like_counts_secs,
            )
            .await;
    }

    /// Publish a like/unlike SSE event to the Redis pub/sub channel for the
    /// given content item. Serializes the strongly-typed `LikeEvent` enum
    /// so the JSON shape is defined in one place (`shared::types`).
    async fn publish_like_event(&self, event: LikeEvent, content_type: &str, content_id: Uuid) {
        let channel = format!("sse:{content_type}:{content_id}");
        if let Ok(json) = serde_json::to_string(&event) {
            self.cache.publish(&channel, &json).await;
        }
    }

    /// Record a like/unlike operation metric.
    ///
    /// Associated function (no `&self`) — metrics are global counters.
    /// `operation` is `&'static str` because callers always pass literals
    /// ("like" / "unlike"), avoiding a `.to_string()` allocation.
    fn record_like_metric(operation: &'static str, content_type: &str) {
        metrics::counter!(
            "social_api_likes_total",
            "content_type" => content_type.to_string(),
            "operation" => operation,
        )
        .increment(1);
    }

    /// Map a sqlx error to AppError::Database.
    /// Centralizes the repeated `.map_err(|e| AppError::Database(e.to_string()))` pattern.
    fn db_err(e: sqlx::Error) -> AppError {
        AppError::Database(e.to_string())
    }

    /// Validate content exists via external Content API (with circuit breaker).
    ///
    /// **Pre-condition:** `content_type` is already validated by the caller.
    async fn validate_content(&self, content_type: &str, content_id: Uuid) -> Result<(), AppError> {
        // Check circuit breaker before making external call
        if !self.content_breaker.allow_request() {
            return Err(AppError::DependencyUnavailable("content_api".to_string()));
        }

        match self
            .content_validator
            .validate(content_type, content_id)
            .await
        {
            Ok(valid) => {
                self.content_breaker.record_success();
                if !valid {
                    return Err(AppError::ContentNotFound {
                        content_type: content_type.to_string(),
                        content_id: content_id.to_string(),
                    });
                }
                Ok(())
            }
            Err(e) => {
                self.content_breaker.record_failure();
                Err(e)
            }
        }
    }
}

/// Build the Redis cache key for a like count: `lc:{content_type}:{content_id}`.
fn count_cache_key(content_type: &str, content_id: Uuid) -> String {
    format!("lc:{content_type}:{content_id}")
}

/// Parse a sorted-set member string `"content_type:content_id"` back into a
/// `TopLikedItem`. Returns `None` (and logs a warning) when the format is
/// unexpected so a single corrupt entry does not break the response.
fn parse_leaderboard_member(member: &str, score: f64) -> Option<TopLikedItem> {
    let colon_pos = member.find(':')?;
    let ct = &member[..colon_pos];
    let cid_str = &member[colon_pos + 1..];

    match Uuid::parse_str(cid_str) {
        Ok(cid) => Some(TopLikedItem {
            content_type: ct.to_string(),
            content_id: cid,
            count: score as i64,
        }),
        Err(e) => {
            tracing::warn!(
                member = member,
                error = %e,
                "Invalid UUID in leaderboard cache member"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use uuid::Uuid;

    // -------------------------------------------------------------------------
    // Test doubles
    // -------------------------------------------------------------------------

    struct AlwaysValidContent;

    #[async_trait::async_trait]
    impl ContentValidator for AlwaysValidContent {
        async fn validate(&self, _ct: &str, _id: Uuid) -> Result<bool, AppError> {
            Ok(true)
        }
    }

    struct AlwaysInvalidContent;

    #[async_trait::async_trait]
    impl ContentValidator for AlwaysInvalidContent {
        async fn validate(&self, _ct: &str, _id: Uuid) -> Result<bool, AppError> {
            Ok(false)
        }
    }

    // -------------------------------------------------------------------------
    // Setup helper (reuses shared containers from test_containers module)
    // -------------------------------------------------------------------------

    async fn make_service(validator: Arc<dyn ContentValidator>) -> LikeService {
        let pg = crate::test_containers::shared_pg().await;
        let redis = crate::test_containers::shared_redis().await;

        let mut config = crate::config::Config::new_for_test();
        config.database_url = pg.url.clone();
        config.read_database_url = pg.url.clone();
        config.redis_url = redis.url.clone();

        let db = crate::db::DbPools::from_config(&config)
            .await
            .expect("db pools");

        let redis_pool = crate::cache::manager::create_pool(&config)
            .await
            .expect("redis pool");
        let cache = crate::cache::manager::CacheManager::new(redis_pool);

        let cb_config = crate::clients::circuit_breaker::CircuitBreakerConfig {
            failure_threshold: 100,
            recovery_timeout: std::time::Duration::from_secs(1),
            success_threshold: 1,
            service_name: "content_api_test".to_string(),
            rate_window: std::time::Duration::from_secs(30),
            failure_rate_threshold: 1.0,
            min_calls_for_rate: 1000,
        };
        let content_breaker = Arc::new(crate::clients::circuit_breaker::CircuitBreaker::new(
            cb_config,
        ));

        LikeService::new_with_validator(db, cache, validator, config, content_breaker)
    }

    // -------------------------------------------------------------------------
    // 1. like new content increments count
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_like_new_content_increments_count() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let resp = svc.like(user_id, "post", content_id).await.unwrap();
        assert!(resp.liked);
        assert_eq!(resp.count, 1);

        let count_resp = svc.get_count("post", content_id).await.unwrap();
        assert_eq!(count_resp.count, 1);
    }

    // -------------------------------------------------------------------------
    // 2. like duplicate is idempotent
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_like_duplicate_is_idempotent() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        svc.like(user_id, "post", content_id).await.unwrap();
        let resp = svc.like(user_id, "post", content_id).await.unwrap();

        assert!(resp.liked);
        assert_eq!(resp.already_existed, Some(true));

        let count_resp = svc.get_count("post", content_id).await.unwrap();
        assert_eq!(count_resp.count, 1);
    }

    // -------------------------------------------------------------------------
    // 3. like invalid content rolls back
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_like_invalid_content_rolls_back() {
        let svc = make_service(Arc::new(AlwaysInvalidContent)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let err = svc.like(user_id, "post", content_id).await.unwrap_err();
        assert!(
            matches!(err, AppError::ContentNotFound { .. }),
            "expected ContentNotFound, got: {err:?}"
        );

        // Count must stay at 0 (row rolled back)
        let count_resp = svc.get_count("post", content_id).await.unwrap();
        assert_eq!(count_resp.count, 0);
    }

    // -------------------------------------------------------------------------
    // 4. unlike decrements count
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_unlike_decrements_count() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        svc.like(user_id, "post", content_id).await.unwrap();

        let resp = svc.unlike(user_id, "post", content_id).await.unwrap();
        assert!(!resp.liked);
        assert_eq!(resp.was_liked, Some(true));

        let count_resp = svc.get_count("post", content_id).await.unwrap();
        assert_eq!(count_resp.count, 0);
    }

    // -------------------------------------------------------------------------
    // 5. unlike not liked is idempotent
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_unlike_not_liked_is_idempotent() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let resp = svc.unlike(user_id, "post", content_id).await.unwrap();
        assert!(!resp.liked);
        assert_eq!(resp.was_liked, Some(false));
    }

    // -------------------------------------------------------------------------
    // 6. get_count cache hit returns same value
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_count_cache_hit() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        svc.like(user_id, "post", content_id).await.unwrap();

        let first = svc.get_count("post", content_id).await.unwrap();
        let second = svc.get_count("post", content_id).await.unwrap();

        assert_eq!(first.count, second.count);
        assert_eq!(first.count, 1);
    }

    // -------------------------------------------------------------------------
    // 7. batch_counts returns correct values
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_batch_counts_returns_correct_values() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let user_id = Uuid::new_v4();
        let content_id_a = Uuid::new_v4();
        let content_id_b = Uuid::new_v4();

        // Like only content A
        svc.like(user_id, "post", content_id_a).await.unwrap();

        let items = vec![
            ("post".to_string(), content_id_a),
            ("post".to_string(), content_id_b),
        ];
        let results = svc.batch_counts(&items).await.unwrap();

        assert_eq!(results.len(), 2);
        let a = results
            .iter()
            .find(|r| r.content_id == content_id_a)
            .unwrap();
        let b = results
            .iter()
            .find(|r| r.content_id == content_id_b)
            .unwrap();
        assert_eq!(a.count, 1);
        assert_eq!(b.count, 0);
    }

    // -------------------------------------------------------------------------
    // 8. batch_counts too large returns error
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_batch_counts_too_large_returns_error() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;

        let items: Vec<(String, Uuid)> = (0..101)
            .map(|_| ("post".to_string(), Uuid::new_v4()))
            .collect();

        let err = svc.batch_counts(&items).await.unwrap_err();
        assert!(
            matches!(
                err,
                AppError::BatchTooLarge {
                    size: 101,
                    max: 100
                }
            ),
            "expected BatchTooLarge, got: {err:?}"
        );
    }

    // -------------------------------------------------------------------------
    // 9. get_leaderboard returns ok
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_leaderboard_returns_ok() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;

        let resp = svc
            .get_leaderboard(None, TimeWindow::All, 10)
            .await
            .unwrap();

        assert_eq!(resp.window, "all");
        // No data yet — items may be empty, but must not panic.
        assert!(resp.items.len() <= 10);
    }

    // -------------------------------------------------------------------------
    // 10. get_user_likes pagination
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_user_likes_pagination() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let user_id = Uuid::new_v4();

        // Like 3 distinct content items
        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        for &cid in &ids {
            svc.like(user_id, "post", cid).await.unwrap();
        }

        // Fetch first page (limit=2)
        let page1 = svc.get_user_likes(user_id, None, None, 2).await.unwrap();
        assert_eq!(page1.items.len(), 2);
        assert!(page1.has_more);
        assert!(page1.next_cursor.is_some());

        // Fetch second page using cursor
        let cursor = page1.next_cursor.as_deref();
        let page2 = svc.get_user_likes(user_id, None, cursor, 2).await.unwrap();
        assert_eq!(page2.items.len(), 1);
        assert!(!page2.has_more);
    }

    // -------------------------------------------------------------------------
    // 11. batch_statuses returns correct status
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_batch_statuses_returns_correct_status() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let user_id = Uuid::new_v4();
        let liked_id = Uuid::new_v4();
        let not_liked_id = Uuid::new_v4();

        svc.like(user_id, "post", liked_id).await.unwrap();

        let items = vec![
            ("post".to_string(), liked_id),
            ("post".to_string(), not_liked_id),
        ];
        let results = svc.batch_statuses(user_id, &items).await.unwrap();

        assert_eq!(results.len(), 2);
        let liked = results.iter().find(|r| r.content_id == liked_id).unwrap();
        let not_liked = results
            .iter()
            .find(|r| r.content_id == not_liked_id)
            .unwrap();
        assert!(liked.liked);
        assert!(liked.liked_at.is_some());
        assert!(!not_liked.liked);
        assert!(not_liked.liked_at.is_none());
    }

    // -------------------------------------------------------------------------
    // 12. stampede coalescing — 5 concurrent get_count calls on cold cache
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_stampede_coalescing() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = Arc::new(svc);
        let content_id = Uuid::new_v4();

        // No likes — cold cache — all concurrent calls should return 0
        let handles: Vec<_> = (0..5)
            .map(|_| {
                let svc = Arc::clone(&svc);
                tokio::spawn(async move { svc.get_count("post", content_id).await })
            })
            .collect();

        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.expect("task did not panic"));
        }

        for r in results {
            assert_eq!(r.unwrap().count, 0);
        }
    }

    // -------------------------------------------------------------------------
    // 13. get_leaderboard with content_type filter (DB fallback path)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_leaderboard_with_content_type_filter() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let user_id = Uuid::new_v4();
        let post_id = Uuid::new_v4();

        // Use a unique content_type so shared-DB data from other tests is excluded
        svc.like(user_id, "lb_svc_test", post_id).await.unwrap();

        // Content_type filter always bypasses cache -> DB fallback
        let resp = svc
            .get_leaderboard(Some("lb_svc_test"), TimeWindow::All, 10)
            .await
            .unwrap();

        assert_eq!(resp.window, "all");
        assert_eq!(resp.content_type, Some("lb_svc_test".to_string()));
        assert!(!resp.items.is_empty(), "expected at least one item");
        assert_eq!(resp.items[0].count, 1);
    }

    // -------------------------------------------------------------------------
    // 14. get_leaderboard from cache (cache-populated path)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_leaderboard_from_cache() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let content_id = Uuid::new_v4();

        // Populate the leaderboard sorted set cache key manually
        let key = "lb:all";
        let member = format!("post:{content_id}");
        svc.cache.replace_sorted_set(key, &[(member, 5.0)]).await;

        // get_leaderboard without filter should use the cache
        let resp = svc
            .get_leaderboard(None, TimeWindow::All, 10)
            .await
            .unwrap();

        assert_eq!(resp.window, "all");
        // Should have at least the item we inserted
        assert!(!resp.items.is_empty());
    }

    // -------------------------------------------------------------------------
    // 15. get_status returns liked status
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_status_liked_and_not_liked() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();
        let other_content_id = Uuid::new_v4();

        svc.like(user_id, "post", content_id).await.unwrap();

        let status = svc.get_status(user_id, "post", content_id).await.unwrap();
        assert!(status.liked);
        assert!(status.liked_at.is_some());

        let status_not_liked = svc
            .get_status(user_id, "post", other_content_id)
            .await
            .unwrap();
        assert!(!status_not_liked.liked);
        assert!(status_not_liked.liked_at.is_none());
    }

    // -------------------------------------------------------------------------
    // 16. get_user_likes with content_type_filter
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_user_likes_with_content_type_filter() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let user_id = Uuid::new_v4();

        let post_id = Uuid::new_v4();
        let bonus_id = Uuid::new_v4();

        svc.like(user_id, "post", post_id).await.unwrap();
        svc.like(user_id, "bonus_hunter", bonus_id).await.unwrap();

        // Filter to posts only
        let page = svc
            .get_user_likes(user_id, Some("post"), None, 10)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].content_type, "post");
        assert!(!page.has_more);
    }

    // -------------------------------------------------------------------------
    // 17. get_user_likes with bad cursor
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_user_likes_bad_cursor_returns_error() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let user_id = Uuid::new_v4();

        let err = svc
            .get_user_likes(user_id, None, Some("NOT_VALID_CURSOR!!"), 10)
            .await
            .unwrap_err();

        assert!(
            matches!(err, AppError::InvalidCursor(_)),
            "expected InvalidCursor, got: {err:?}"
        );
    }

    // -------------------------------------------------------------------------
    // 18. batch_statuses too large returns error
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_batch_statuses_too_large_returns_error() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let user_id = Uuid::new_v4();

        let items: Vec<(String, Uuid)> = (0..101)
            .map(|_| ("post".to_string(), Uuid::new_v4()))
            .collect();

        let err = svc.batch_statuses(user_id, &items).await.unwrap_err();
        assert!(
            matches!(
                err,
                AppError::BatchTooLarge {
                    size: 101,
                    max: 100
                }
            ),
            "expected BatchTooLarge, got: {err:?}"
        );
    }

    // -------------------------------------------------------------------------
    // 19. circuit breaker open rejects like
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_circuit_breaker_open_rejects_like() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        // Trip the circuit breaker by recording enough failures
        for _ in 0..105 {
            svc.content_breaker.record_failure();
        }

        // Circuit should now be Open — like should fail with DependencyUnavailable
        let err = svc.like(user_id, "post", content_id).await.unwrap_err();
        assert!(
            matches!(err, AppError::DependencyUnavailable(_)),
            "expected DependencyUnavailable, got: {err:?}"
        );
    }

    // -------------------------------------------------------------------------
    // 20. validate_content when validator returns error (not just false)
    // -------------------------------------------------------------------------

    struct AlwaysErrorContent;

    #[async_trait::async_trait]
    impl ContentValidator for AlwaysErrorContent {
        async fn validate(&self, _ct: &str, _id: Uuid) -> Result<bool, AppError> {
            Err(AppError::DependencyUnavailable("content_api".to_string()))
        }
    }

    #[tokio::test]
    async fn test_content_validator_error_rolls_back_and_records_failure() {
        let svc = make_service(Arc::new(AlwaysErrorContent)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let err = svc.like(user_id, "post", content_id).await.unwrap_err();
        assert!(
            matches!(err, AppError::DependencyUnavailable(_)),
            "expected DependencyUnavailable, got: {err:?}"
        );

        // Count should remain 0 (row rolled back)
        let count_resp = svc.get_count("post", content_id).await.unwrap();
        assert_eq!(count_resp.count, 0);
    }

    // -------------------------------------------------------------------------
    // 21. get_leaderboard time windows
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_leaderboard_different_time_windows() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;

        for window in [
            TimeWindow::Day,
            TimeWindow::Week,
            TimeWindow::Month,
            TimeWindow::All,
        ] {
            let resp = svc.get_leaderboard(None, window, 5).await.unwrap();
            assert_eq!(resp.window, window.as_str());
        }
    }

    // -------------------------------------------------------------------------
    // 22. batch_counts with all cache hits
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_batch_counts_all_cache_hits() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        // Like to populate cache
        svc.like(user_id, "post", content_id).await.unwrap();

        // Call get_count to ensure cache is warm
        svc.get_count("post", content_id).await.unwrap();

        // Batch count with cached item
        let items = vec![("post".to_string(), content_id)];
        let results = svc.batch_counts(&items).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].count, 1);
    }

    // -------------------------------------------------------------------------
    // 23. unlike content that is not liked (was_liked=false branch for cache)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_unlike_not_liked_get_count_inner_path() {
        let svc = make_service(Arc::new(AlwaysValidContent)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        // Unlike something not liked - exercises get_count_inner path
        let resp = svc.unlike(user_id, "post", content_id).await.unwrap();
        assert_eq!(resp.was_liked, Some(false));
        assert_eq!(resp.count, 0);
    }
}
