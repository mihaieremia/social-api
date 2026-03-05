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
    /// Trait object — swappable transport (HTTP today, gRPC tomorrow).
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

    /// Test constructor — accepts a pre-built ContentValidator trait object.
    /// Production code uses `new()` which builds `HttpContentValidator`.
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
        // Insert first to determine if this is a new or duplicate request.
        // Duplicate detection must happen before content validation so we can
        // skip the external HTTP call for requests we've already processed.
        let (like_row, already_existed) =
            like_repository::insert_like(&self.db.writer, user_id, content_type, content_id)
                .await
                .map_err(Self::db_err)?;

        if !already_existed {
            // Content validation is skipped for duplicate likes.
            // Rationale: if this like already exists, the content was valid at the time it was
            // first created. Re-validating on every duplicate request would fire a redundant
            // external HTTP round-trip with no correctness benefit.
            //
            // On validation failure for a NEW like: undo the insert so the DB stays consistent.
            // Best-effort rollback — if delete_like also fails, the orphaned row will have
            // count=1 with no valid content. Acceptable: content APIs are expected to be available.
            if let Err(e) = self.validate_content(content_type, content_id).await {
                let _ = like_repository::delete_like(
                    &self.db.writer,
                    user_id,
                    content_type,
                    content_id,
                )
                .await;
                return Err(e);
            }
        }

        let count = if !already_existed {
            let cache_key = format!("lc:{content_type}:{content_id}");
            let db_count = like_repository::get_count(&self.db.reader, content_type, content_id)
                .await
                .map_err(Self::db_err)?;
            self.update_count_cache(&cache_key, 1, db_count).await
        } else {
            self.get_count_inner(content_type, content_id).await?
        };

        if !already_existed {
            let event = serde_json::json!({
                "event": "like",
                "user_id": user_id,
                "count": count,
                "timestamp": like_row.created_at.to_rfc3339(),
            });
            let channel = format!("sse:{content_type}:{content_id}");
            self.cache.publish(&channel, &event.to_string()).await;
        }

        Ok(LikeActionResponse {
            liked: true,
            already_existed: Some(already_existed),
            was_liked: None,
            count,
            liked_at: Some(like_row.created_at),
        })
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
        let was_liked =
            like_repository::delete_like(&self.db.writer, user_id, content_type, content_id)
                .await
                .map_err(Self::db_err)?;

        // Update cache with conditional DECR (safe against expired keys)
        let count = if was_liked {
            let cache_key = format!("lc:{content_type}:{content_id}");
            let db_count = like_repository::get_count(&self.db.reader, content_type, content_id)
                .await
                .map_err(Self::db_err)?;
            self.update_count_cache(&cache_key, -1, db_count).await
        } else {
            self.get_count_inner(content_type, content_id).await?
        };

        // Publish SSE event
        if was_liked {
            let event = serde_json::json!({
                "event": "unlike",
                "user_id": user_id,
                "count": count,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            });
            let channel = format!("sse:{content_type}:{content_id}");
            self.cache.publish(&channel, &event.to_string()).await;
        }

        Ok(LikeActionResponse {
            liked: false,
            already_existed: None,
            was_liked: Some(was_liked),
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
    /// On a cache miss, the first task to notice registers a `watch::Sender` in
    /// `pending_fetches` and fetches from DB. Every concurrent waiter subscribes
    /// to that sender and is woken immediately when the result is ready — no
    /// fixed sleep delay. If the fetcher fails, the sender drops, waiters
    /// receive `Err` on `changed()` and fall back to DB directly.
    async fn get_count_inner(&self, content_type: &str, content_id: Uuid) -> Result<i64, AppError> {
        let cache_key = format!("lc:{content_type}:{content_id}");

        // Fast path: cache hit
        if let Some(cached) = self.cache.get(&cache_key).await
            && let Ok(count) = cached.parse::<i64>()
        {
            return Ok(count);
        }

        // Stampede protection: if another task is already fetching this key,
        // subscribe to its result instead of firing a duplicate DB query.
        if let Some(sender) = self.pending_fetches.get(&cache_key) {
            let mut rx = sender.subscribe();
            // Release the DashMap shard lock before awaiting — holding it across
            // an await point would block other tasks from reading the same shard.
            drop(sender);
            rx.changed().await.ok();
            if let Some(count) = *rx.borrow() {
                return Ok(count);
            }
            // Sender was dropped (fetcher task failed) — fall through to DB directly.
        }

        // We are the designated fetcher. Register a channel so concurrent
        // waiters can subscribe to our result.
        let (tx, _rx) = watch::channel(None::<i64>);
        self.pending_fetches.insert(cache_key.clone(), tx.clone());

        let count = like_repository::get_count(&self.db.reader, content_type, content_id)
            .await
            .map_err(Self::db_err)?;

        self.cache
            .set(
                &cache_key,
                &count.to_string(),
                self.config.cache_ttl_like_counts_secs,
            )
            .await;

        // Wake all waiters immediately with the fetched count.
        tx.send(Some(count)).ok();
        self.pending_fetches.remove(&cache_key);

        Ok(count)
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
            cache_keys.push(format!("lc:{ct}:{cid}"));
        }

        let cached_values = self.cache.mget(&cache_keys).await;

        let mut results = Vec::with_capacity(items.len());
        let mut missing_indices = Vec::with_capacity(items.len());

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
            missing_indices.push(i);
            results.push(BatchCountResult {
                content_type: ct.clone(),
                content_id: *cid,
                count: 0,
            });
        }

        // Fetch missing from DB
        if !missing_indices.is_empty() {
            let mut missing_items = Vec::with_capacity(missing_indices.len());
            for &i in &missing_indices {
                missing_items.push(items[i].clone());
            }

            // Build O(1) lookup index: (content_type, content_id) -> results position.
            let index: std::collections::HashMap<(String, Uuid), usize> = items
                .iter()
                .enumerate()
                .map(|(i, (ct, cid))| ((ct.clone(), *cid), i))
                .collect();

            let db_counts = like_repository::batch_get_counts(&self.db.reader, &missing_items)
                .await
                .map_err(Self::db_err)?;

            // Update results and collect cache entries for pipeline
            let mut cache_entries = Vec::with_capacity(db_counts.len());
            for (ct, cid, count) in db_counts {
                if let Some(&pos) = index.get(&(ct.clone(), cid)) {
                    results[pos].count = count;
                }
                cache_entries.push((format!("lc:{ct}:{cid}"), count.to_string()));
            }

            // Populate cache in a single pipeline round-trip
            if !cache_entries.is_empty() {
                let ttl = self.config.cache_ttl_like_counts_secs;
                let mut entries = Vec::with_capacity(cache_entries.len());
                for (k, v) in &cache_entries {
                    entries.push((k.as_str(), v.as_str(), ttl));
                }
                self.cache.mset_ex(&entries).await;
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

    /// Map a sqlx error to AppError::Database.
    /// Centralizes the repeated `.map_err(|e| AppError::Database(e.to_string()))` pattern.
    fn db_err(e: sqlx::Error) -> AppError {
        AppError::Database(e.to_string())
    }

    /// Apply a conditional INCR (delta > 0) or DECR (delta < 0) to the count cache.
    /// Returns the new count, falling back to `db_count` on cache error.
    async fn update_count_cache(&self, key: &str, delta: i64, db_count: i64) -> i64 {
        let ttl = self.config.cache_ttl_like_counts_secs;
        let result = if delta > 0 {
            self.cache.conditional_incr(key, ttl, db_count).await
        } else {
            self.cache.conditional_decr(key, ttl, db_count).await
        };
        result.unwrap_or(db_count)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use testcontainers::runners::AsyncRunner;
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
    // Setup helper
    // -------------------------------------------------------------------------

    /// Start postgres + redis testcontainers and return a LikeService + the
    /// containers (which must be kept alive for the duration of the test).
    async fn make_service(
        validator: Arc<dyn ContentValidator>,
    ) -> (
        LikeService,
        testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
        testcontainers::ContainerAsync<testcontainers_modules::redis::Redis>,
    ) {
        // Postgres
        let pg = testcontainers_modules::postgres::Postgres::default()
            .start()
            .await
            .expect("postgres container");
        let pg_port = pg.get_host_port_ipv4(5432).await.unwrap();
        let db_url = format!(
            "postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres"
        );

        // Redis
        let redis = testcontainers_modules::redis::Redis::default()
            .start()
            .await
            .expect("redis container");
        let redis_port = redis.get_host_port_ipv4(6379).await.unwrap();
        let redis_url = format!("redis://127.0.0.1:{redis_port}");

        // Config
        let mut config = crate::config::Config::new_for_test();
        config.database_url = db_url.clone();
        config.read_database_url = db_url.clone();
        config.redis_url = redis_url;

        // DB pools
        let db = crate::db::DbPools::from_config(&config)
            .await
            .expect("db pools");

        // Run migrations
        sqlx::migrate!("../../migrations")
            .run(&db.writer)
            .await
            .expect("migrations");

        // Redis pool + cache
        let redis_pool = crate::cache::manager::create_pool(&config)
            .await
            .expect("redis pool");
        let cache = crate::cache::manager::CacheManager::new(redis_pool);

        // Circuit breaker (permissive for tests)
        let cb_config = crate::clients::circuit_breaker::CircuitBreakerConfig {
            failure_threshold: 100,
            recovery_timeout: std::time::Duration::from_secs(1),
            success_threshold: 1,
            service_name: "content_api_test".to_string(),
            rate_window: std::time::Duration::from_secs(30),
            failure_rate_threshold: 1.0,
            min_calls_for_rate: 1000,
        };
        let content_breaker = Arc::new(crate::clients::circuit_breaker::CircuitBreaker::new(cb_config));

        let svc = LikeService::new_with_validator(db, cache, validator, config, content_breaker);
        (svc, pg, redis)
    }

    // -------------------------------------------------------------------------
    // 1. like new content increments count
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_like_new_content_increments_count() {
        let (svc, _pg, _redis) = make_service(Arc::new(AlwaysValidContent)).await;
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
        let (svc, _pg, _redis) = make_service(Arc::new(AlwaysValidContent)).await;
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
        let (svc, _pg, _redis) = make_service(Arc::new(AlwaysInvalidContent)).await;
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
        let (svc, _pg, _redis) = make_service(Arc::new(AlwaysValidContent)).await;
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
        let (svc, _pg, _redis) = make_service(Arc::new(AlwaysValidContent)).await;
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
        let (svc, _pg, _redis) = make_service(Arc::new(AlwaysValidContent)).await;
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
        let (svc, _pg, _redis) = make_service(Arc::new(AlwaysValidContent)).await;
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
        let a = results.iter().find(|r| r.content_id == content_id_a).unwrap();
        let b = results.iter().find(|r| r.content_id == content_id_b).unwrap();
        assert_eq!(a.count, 1);
        assert_eq!(b.count, 0);
    }

    // -------------------------------------------------------------------------
    // 8. batch_counts too large returns error
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_batch_counts_too_large_returns_error() {
        let (svc, _pg, _redis) = make_service(Arc::new(AlwaysValidContent)).await;

        let items: Vec<(String, Uuid)> = (0..101)
            .map(|_| ("post".to_string(), Uuid::new_v4()))
            .collect();

        let err = svc.batch_counts(&items).await.unwrap_err();
        assert!(
            matches!(err, AppError::BatchTooLarge { size: 101, max: 100 }),
            "expected BatchTooLarge, got: {err:?}"
        );
    }

    // -------------------------------------------------------------------------
    // 9. get_leaderboard returns ok
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_leaderboard_returns_ok() {
        let (svc, _pg, _redis) = make_service(Arc::new(AlwaysValidContent)).await;

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
        let (svc, _pg, _redis) = make_service(Arc::new(AlwaysValidContent)).await;
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
        let (svc, _pg, _redis) = make_service(Arc::new(AlwaysValidContent)).await;
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
        let not_liked = results.iter().find(|r| r.content_id == not_liked_id).unwrap();
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
        let (svc, _pg, _redis) = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = Arc::new(svc);
        let content_id = Uuid::new_v4();

        // No likes — cold cache — all concurrent calls should return 0
        let handles: Vec<_> = (0..5)
            .map(|_| {
                let svc = Arc::clone(&svc);
                tokio::spawn(async move {
                    svc.get_count("post", content_id).await
                })
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
