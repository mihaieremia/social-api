use chrono::{Duration, Utc};
use shared::errors::AppError;
use shared::types::*;
use uuid::Uuid;

use crate::cache::manager::CacheManager;
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
}

impl LikeService {
    pub fn new(
        db: DbPools,
        cache: CacheManager,
        http_client: reqwest::Client,
        config: Config,
    ) -> Self {
        let content_validator =
            HttpContentValidator::new(http_client, cache.clone(), config.clone());
        Self {
            db,
            cache,
            content_validator,
            config,
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

        // Update cache
        if !already_existed {
            let cache_key = format!("lc:{content_type}:{content_id}");
            self.cache.incr(&cache_key).await;
        }

        // Get current count
        let count = self.get_count_inner(content_type, content_id).await?;

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

        // Update cache
        if was_liked {
            let cache_key = format!("lc:{content_type}:{content_id}");
            self.cache.decr(&cache_key).await;
        }

        let count = self.get_count_inner(content_type, content_id).await?;

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
    async fn get_count_inner(
        &self,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<i64, AppError> {
        let cache_key = format!("lc:{content_type}:{content_id}");

        // Try cache first
        if let Some(cached) = self.cache.get(&cache_key).await {
            if let Ok(count) = cached.parse::<i64>() {
                return Ok(count);
            }
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

            Ok(count)
        } else {
            // Someone else is fetching — wait briefly then retry cache
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            if let Some(cached) = self.cache.get(&cache_key).await {
                if let Ok(count) = cached.parse::<i64>() {
                    return Ok(count);
                }
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
        let limit = limit.min(100).max(1);

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
        let items: Vec<UserLikeItem> = rows
            .iter()
            .take(limit as usize)
            .map(|r| UserLikeItem {
                content_type: r.content_type.clone(),
                content_id: r.content_id,
                liked_at: r.created_at,
            })
            .collect();

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
        let cache_keys: Vec<String> = items
            .iter()
            .map(|(ct, cid)| format!("lc:{ct}:{cid}"))
            .collect();

        let cached_values = self.cache.mget(&cache_keys).await;

        let mut results = Vec::with_capacity(items.len());
        let mut missing_indices = Vec::new();

        for (i, (ct, cid)) in items.iter().enumerate() {
            if let Some(Some(val)) = cached_values.get(i) {
                if let Ok(count) = val.parse::<i64>() {
                    results.push(BatchCountResult {
                        content_type: ct.clone(),
                        content_id: *cid,
                        count,
                    });
                    continue;
                }
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
            let missing_items: Vec<(String, Uuid)> = missing_indices
                .iter()
                .map(|&i| items[i].clone())
                .collect();

            let db_counts =
                like_repository::batch_get_counts(&self.db.reader, &missing_items)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;

            // Update results and cache
            for (ct, cid, count) in db_counts {
                if let Some(pos) = results
                    .iter()
                    .position(|r| r.content_type == ct && r.content_id == cid)
                {
                    results[pos].count = count;
                }
                // Populate cache
                let cache_key = format!("lc:{ct}:{cid}");
                self.cache
                    .set(
                        &cache_key,
                        &count.to_string(),
                        self.config.cache_ttl_like_counts_secs,
                    )
                    .await;
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
        let mut result_map: std::collections::HashMap<(String, Uuid), Option<chrono::DateTime<Utc>>> =
            rows.into_iter().map(|(ct, cid, ts)| ((ct, cid), ts)).collect();

        // Ensure all requested items appear in results
        let results: Vec<BatchStatusResult> = items
            .iter()
            .map(|(ct, cid)| {
                let liked_at = result_map
                    .remove(&(ct.clone(), *cid))
                    .flatten();
                BatchStatusResult {
                    content_type: ct.clone(),
                    content_id: *cid,
                    liked: liked_at.is_some(),
                    liked_at,
                }
            })
            .collect();

        Ok(results)
    }

    /// Get top liked content leaderboard.
    pub async fn get_leaderboard(
        &self,
        content_type: Option<&str>,
        window: TimeWindow,
        limit: i64,
    ) -> Result<TopLikedResponse, AppError> {
        let limit = limit.min(50).max(1);

        let since = window.duration_secs().map(|secs| {
            Utc::now() - Duration::seconds(secs)
        });

        let rows =
            like_repository::get_leaderboard(&self.db.reader, content_type, since, limit)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;

        let items: Vec<TopLikedItem> = rows
            .into_iter()
            .map(|(ct, cid, count)| TopLikedItem {
                content_type: ct,
                content_id: cid,
                count,
            })
            .collect();

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

    async fn validate_content(
        &self,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<(), AppError> {
        let valid = self
            .content_validator
            .validate(content_type, content_id)
            .await?;

        if !valid {
            return Err(AppError::ContentNotFound {
                content_type: content_type.to_string(),
                content_id: content_id.to_string(),
            });
        }
        Ok(())
    }
}
