use std::sync::Arc;

use chrono::{Duration, Utc};
use shared::errors::AppError;
use shared::types::*;
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
    content_validator: Arc<dyn ContentValidator>,
    config: Config,
    content_breaker: Arc<CircuitBreaker>,
}

impl LikeService {
    pub fn new(
        db: DbPools,
        cache: CacheManager,
        http_client: reqwest::Client,
        config: Config,
        content_breaker: Arc<CircuitBreaker>,
    ) -> Self {
        let content_validator: Arc<dyn ContentValidator> = Arc::new(
            HttpContentValidator::new(http_client, cache.clone(), config.clone()),
        );
        Self {
            db,
            cache,
            content_validator,
            config,
            content_breaker,
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
        // Validate content exists via external Content API
        self.validate_content(content_type, content_id).await?;

        // Insert like (idempotent)
        let (like_row, already_existed) =
            like_repository::insert_like(&self.db.writer, user_id, content_type, content_id)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;

        // Update cache with conditional INCR (safe against expired keys)
        let count = if !already_existed {
            let cache_key = format!("lc:{content_type}:{content_id}");
            // Fetch DB count as fallback for conditional INCR
            let db_count = like_repository::get_count(&self.db.reader, content_type, content_id)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
            self.cache
                .conditional_incr(&cache_key, self.config.cache_ttl_like_counts_secs, db_count)
                .await
                .unwrap_or(db_count)
        } else {
            self.get_count_inner(content_type, content_id).await?
        };

        // Publish SSE event
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
                .map_err(|e| AppError::Database(e.to_string()))?;

        // Update cache with conditional DECR (safe against expired keys)
        let count = if was_liked {
            let cache_key = format!("lc:{content_type}:{content_id}");
            let db_count = like_repository::get_count(&self.db.reader, content_type, content_id)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
            self.cache
                .conditional_decr(&cache_key, self.config.cache_ttl_like_counts_secs, db_count)
                .await
                .unwrap_or(db_count)
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

    /// Internal count getter with cache-first + stampede protection.
    async fn get_count_inner(&self, content_type: &str, content_id: Uuid) -> Result<i64, AppError> {
        let cache_key = format!("lc:{content_type}:{content_id}");

        // Try cache first
        if let Some(cached) = self.cache.get(&cache_key).await
            && let Ok(count) = cached.parse::<i64>()
        {
            return Ok(count);
        }

        // Stampede protection: try to acquire lock
        let lock_key = format!("{cache_key}:lock");
        let acquired = self.cache.set_nx(&lock_key, "1", 5).await;

        if acquired {
            // We got the lock — fetch from DB and populate cache
            let count = like_repository::get_count(&self.db.reader, content_type, content_id)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;

            self.cache
                .set(
                    &cache_key,
                    &count.to_string(),
                    self.config.cache_ttl_like_counts_secs,
                )
                .await;

            // Release lock immediately so other waiters can read from cache
            self.cache.del(&lock_key).await;

            Ok(count)
        } else {
            // Someone else is fetching — wait briefly then retry cache
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            if let Some(cached) = self.cache.get(&cache_key).await
                && let Ok(count) = cached.parse::<i64>()
            {
                return Ok(count);
            }

            // Fallback to DB directly
            like_repository::get_count(&self.db.reader, content_type, content_id)
                .await
                .map_err(|e| AppError::Database(e.to_string()))
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
                .map_err(|e| AppError::Database(e.to_string()))?;

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
        .map_err(|e| AppError::Database(e.to_string()))?;

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

            let db_counts = like_repository::batch_get_counts(&self.db.reader, &missing_items)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;

            // Update results and collect cache entries for pipeline
            let mut cache_entries = Vec::with_capacity(db_counts.len());
            for (ct, cid, count) in db_counts {
                if let Some(pos) = results
                    .iter()
                    .position(|r| r.content_type == ct && r.content_id == cid)
                {
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
            .map_err(|e| AppError::Database(e.to_string()))?;

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
            .map_err(|e| AppError::Database(e.to_string()))?;

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
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::{postgres::Postgres, redis::Redis};
    use uuid::Uuid;

    use crate::cache::manager::{CacheManager, create_pool};
    use crate::clients::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
    use crate::clients::content_client::ContentValidator;
    use crate::config::Config;
    use crate::db::DbPools;
    use shared::errors::AppError;

    // --- Content validator stubs ---

    struct AlwaysValid;
    #[async_trait::async_trait]
    impl ContentValidator for AlwaysValid {
        async fn validate(&self, _: &str, _: Uuid) -> Result<bool, AppError> {
            Ok(true)
        }
    }

    struct AlwaysNotFound;
    #[async_trait::async_trait]
    impl ContentValidator for AlwaysNotFound {
        async fn validate(&self, _: &str, _: Uuid) -> Result<bool, AppError> {
            Ok(false)
        }
    }

    // --- Container bootstrap helper ---

    struct TestInfra {
        pub service: LikeService,
        // Containers held here — dropped (stopped) when TestInfra is dropped
        _pg: testcontainers::ContainerAsync<Postgres>,
        _redis: testcontainers::ContainerAsync<Redis>,
    }

    async fn setup(validator: Arc<dyn ContentValidator>) -> TestInfra {
        let pg = Postgres::default().start().await.expect("postgres container");
        let redis = Redis::default().start().await.expect("redis container");

        let pg_port = pg.get_host_port_ipv4(5432).await.unwrap();
        let redis_port = redis.get_host_port_ipv4(6379).await.unwrap();

        let db_url = format!("postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres");

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&db_url)
            .await
            .expect("connect test postgres");

        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("run migrations");

        let db = DbPools {
            writer: pool.clone(),
            reader: pool,
        };

        let mut config = Config::new_for_test();
        config.redis_url = format!("redis://127.0.0.1:{redis_port}");

        let redis_pool = create_pool(&config).await.expect("redis pool");
        let cache = CacheManager::new(redis_pool);

        let breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 5,
            recovery_timeout: std::time::Duration::from_secs(30),
            success_threshold: 2,
            service_name: "content_api_test".to_string(),
            rate_window: std::time::Duration::from_secs(30),
            failure_rate_threshold: 0.5,
            min_calls_for_rate: 10,
        }));

        let service = LikeService {
            db,
            cache,
            content_validator: validator,
            config,
            content_breaker: breaker,
        };

        TestInfra {
            service,
            _pg: pg,
            _redis: redis,
        }
    }

    // --- Tests ---

    #[tokio::test]
    async fn test_like_new_content_returns_count_one() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let resp = infra
            .service
            .like(Uuid::new_v4(), "post", Uuid::new_v4())
            .await
            .unwrap();

        assert!(resp.liked);
        assert_eq!(resp.already_existed, Some(false));
        assert_eq!(resp.count, 1);
        assert!(resp.liked_at.is_some());
    }

    #[tokio::test]
    async fn test_like_same_content_twice_is_idempotent() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        infra.service.like(user_id, "post", content_id).await.unwrap();
        let resp = infra.service.like(user_id, "post", content_id).await.unwrap();

        assert_eq!(resp.already_existed, Some(true));
        assert_eq!(resp.count, 1, "count must not increase on duplicate");
    }

    #[tokio::test]
    async fn test_unlike_not_liked_returns_was_liked_false() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let resp = infra
            .service
            .unlike(Uuid::new_v4(), "post", Uuid::new_v4())
            .await
            .unwrap();

        assert!(!resp.liked);
        assert_eq!(resp.was_liked, Some(false));
    }

    #[tokio::test]
    async fn test_like_then_unlike_count_returns_to_zero() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        infra.service.like(user_id, "post", content_id).await.unwrap();
        let resp = infra
            .service
            .unlike(user_id, "post", content_id)
            .await
            .unwrap();

        assert_eq!(resp.count, 0);
        assert_eq!(resp.was_liked, Some(true));
    }

    #[tokio::test]
    async fn test_get_count_cache_hit_returns_cached_value() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let content_id = Uuid::new_v4();

        // Pre-seed cache with a known value (bypassing DB entirely)
        let cache_key = format!("lc:post:{content_id}");
        infra.service.cache.set(&cache_key, "42", 300).await;

        let resp = infra.service.get_count("post", content_id).await.unwrap();
        assert_eq!(resp.count, 42, "must return cached value, not DB count of 0");
    }

    #[tokio::test]
    async fn test_get_count_cache_miss_falls_back_to_db() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        // Like to create a DB record, clear cache to force DB fallback
        infra.service.like(user_id, "post", content_id).await.unwrap();
        infra
            .service
            .cache
            .del(&format!("lc:post:{content_id}"))
            .await;

        let resp = infra.service.get_count("post", content_id).await.unwrap();
        assert_eq!(resp.count, 1, "must fall back to DB count");
    }

    #[tokio::test]
    async fn test_content_not_found_returns_error() {
        let infra = setup(Arc::new(AlwaysNotFound)).await;
        let err = infra
            .service
            .like(Uuid::new_v4(), "post", Uuid::new_v4())
            .await
            .unwrap_err();

        assert!(matches!(err, AppError::ContentNotFound { .. }));
    }

    #[tokio::test]
    async fn test_batch_counts_over_100_returns_error() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let items: Vec<(String, Uuid)> = (0..101)
            .map(|_| ("post".to_string(), Uuid::new_v4()))
            .collect();

        let err = infra.service.batch_counts(&items).await.unwrap_err();
        assert!(matches!(
            err,
            AppError::BatchTooLarge { size: 101, max: 100 }
        ));
    }

    #[tokio::test]
    async fn test_batch_statuses_over_100_returns_error() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let items: Vec<(String, Uuid)> = (0..101)
            .map(|_| ("post".to_string(), Uuid::new_v4()))
            .collect();

        let err = infra
            .service
            .batch_statuses(Uuid::new_v4(), &items)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            AppError::BatchTooLarge { size: 101, max: 100 }
        ));
    }

    #[tokio::test]
    async fn test_batch_counts_cache_miss_fetches_from_db() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let content_id = Uuid::new_v4();

        // Create 2 likes via the service (writes to DB + populates cache)
        for _ in 0..2 {
            infra
                .service
                .like(Uuid::new_v4(), "post", content_id)
                .await
                .unwrap();
        }
        // Clear cache to force DB fallback in batch_counts
        infra
            .service
            .cache
            .del(&format!("lc:post:{content_id}"))
            .await;

        let results = infra
            .service
            .batch_counts(&[("post".to_string(), content_id)])
            .await
            .unwrap();

        assert_eq!(results[0].count, 2);
    }

    #[tokio::test]
    async fn test_get_leaderboard_reads_from_zset_cache() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let content_id = Uuid::new_v4();

        // Seed leaderboard ZSET directly
        infra
            .service
            .cache
            .replace_sorted_set("lb:all", &[(format!("post:{content_id}"), 7.0)])
            .await;

        let resp = infra
            .service
            .get_leaderboard(None, shared::types::TimeWindow::All, 10)
            .await
            .unwrap();

        assert_eq!(resp.items.len(), 1);
        assert_eq!(resp.items[0].count, 7);
        assert_eq!(resp.items[0].content_id, content_id);
    }

    #[tokio::test]
    async fn test_get_leaderboard_falls_back_to_db_on_empty_zset() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let content_id = Uuid::new_v4();

        // Like 3 times — no ZSET seeded, so must fall back to DB
        for _ in 0..3 {
            infra
                .service
                .like(Uuid::new_v4(), "post", content_id)
                .await
                .unwrap();
        }

        let resp = infra
            .service
            .get_leaderboard(None, shared::types::TimeWindow::All, 10)
            .await
            .unwrap();

        assert!(!resp.items.is_empty());
        assert_eq!(resp.items[0].count, 3);
    }

    #[tokio::test]
    async fn test_get_user_likes_cursor_pagination() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let user_id = Uuid::new_v4();

        for _ in 0..5 {
            infra
                .service
                .like(user_id, "post", Uuid::new_v4())
                .await
                .unwrap();
        }

        let page1 = infra
            .service
            .get_user_likes(user_id, None, None, 2)
            .await
            .unwrap();

        assert_eq!(page1.items.len(), 2);
        assert!(page1.has_more);
        assert!(page1.next_cursor.is_some());

        let page2 = infra
            .service
            .get_user_likes(user_id, None, page1.next_cursor.as_deref(), 2)
            .await
            .unwrap();

        assert!(!page2.items.is_empty());
        let page1_ids: std::collections::HashSet<_> =
            page1.items.iter().map(|i| i.content_id).collect();
        for item in &page2.items {
            assert!(!page1_ids.contains(&item.content_id));
        }
    }

    #[tokio::test]
    async fn test_circuit_breaker_open_returns_dependency_unavailable() {
        let infra = setup(Arc::new(AlwaysValid)).await;

        // Trip the breaker manually by recording failures
        for _ in 0..5 {
            infra.service.content_breaker.record_failure();
        }

        let err = infra
            .service
            .like(Uuid::new_v4(), "post", Uuid::new_v4())
            .await
            .unwrap_err();

        assert!(matches!(err, AppError::DependencyUnavailable(_)));
    }
}
