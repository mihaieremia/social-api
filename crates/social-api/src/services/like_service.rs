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
    content_validator: HttpContentValidator,
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
        let content_validator =
            HttpContentValidator::new(http_client, cache.clone(), config.clone());
        Self {
            db,
            cache,
            content_validator,
            config,
            content_breaker,
        }
    }

    /// Like content. Idempotent — duplicate requests return success.
    pub async fn like(
        &self,
        user_id: Uuid,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeActionResponse, AppError> {
        // Validate content type
        self.validate_content_type(content_type)?;

        // Validate content exists
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
            let db_count =
                like_repository::get_count(&self.db.reader, content_type, content_id)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
            self.cache
                .conditional_incr(
                    &cache_key,
                    self.config.cache_ttl_like_counts_secs,
                    db_count,
                )
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
    pub async fn unlike(
        &self,
        user_id: Uuid,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeActionResponse, AppError> {
        self.validate_content_type(content_type)?;

        let was_liked =
            like_repository::delete_like(&self.db.writer, user_id, content_type, content_id)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;

        // Update cache with conditional DECR (safe against expired keys)
        let count = if was_liked {
            let cache_key = format!("lc:{content_type}:{content_id}");
            let db_count =
                like_repository::get_count(&self.db.reader, content_type, content_id)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
            self.cache
                .conditional_decr(
                    &cache_key,
                    self.config.cache_ttl_like_counts_secs,
                    db_count,
                )
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
    pub async fn get_count(
        &self,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeCountResponse, AppError> {
        self.validate_content_type(content_type)?;

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
    pub async fn get_status(
        &self,
        user_id: Uuid,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeStatusResponse, AppError> {
        self.validate_content_type(content_type)?;

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

    fn validate_content_type(&self, content_type: &str) -> Result<(), AppError> {
        if !self.config.is_valid_content_type(content_type) {
            return Err(AppError::ContentTypeUnknown(content_type.to_string()));
        }
        Ok(())
    }

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
