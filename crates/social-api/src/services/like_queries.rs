use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use shared::errors::AppError;
use shared::types::{
    BatchCountResult, BatchStatusResult, LikeCountResponse, LikeStatusResponse, PaginatedResponse,
    TimeWindow, TopLikedItem, TopLikedResponse, UserLikeItem,
};
use uuid::Uuid;

use crate::cache::CacheManager;
use crate::config::Config;
use crate::db::DbPools;
use crate::repositories;
use crate::services::like_count_cache::{LikeCountCache, map_db_error};

/// Cached first page of a user's likes.
/// Stores up to 101 LikeRows (limit=100 + 1 for has_more detection at max).
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedUserLikePage {
    rows: Vec<repositories::LikeRow>,
    has_more: bool,
}

pub(crate) struct LikeQueryService {
    db: DbPools,
    cache: CacheManager,
    count_cache: Arc<LikeCountCache>,
    config: Config,
}

impl LikeQueryService {
    pub(crate) fn new(
        db: DbPools,
        cache: CacheManager,
        count_cache: Arc<LikeCountCache>,
        config: Config,
    ) -> Self {
        Self {
            db,
            cache,
            count_cache,
            config,
        }
    }

    pub(crate) async fn get_count(
        &self,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeCountResponse, AppError> {
        let count = self
            .count_cache
            .get_count_value(content_type, content_id)
            .await?;

        Ok(LikeCountResponse {
            content_type: content_type.to_string(),
            content_id,
            count,
        })
    }

    /// Get like status for a single user+content pair.
    /// Checks L1 → Redis → DB. Caches the result for subsequent requests.
    pub(crate) async fn get_status(
        &self,
        user_id: Uuid,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeStatusResponse, AppError> {
        let cache_key = status_cache_key(user_id, content_type, content_id);

        // Cache hit: parse stored value
        if let Some(cached) = self.cache.get(&cache_key).await {
            return Ok(parse_status_cache_value(&cached));
        }

        // Cache miss: query DB
        let liked_at =
            repositories::get_like_status(&self.db.reader, user_id, content_type, content_id)
                .await
                .map_err(map_db_error)?;

        // Write-through: cache the result (timestamp for liked, "" for not liked)
        let value = match &liked_at {
            Some(ts) => ts.to_rfc3339(),
            None => String::new(),
        };
        self.cache
            .set(&cache_key, &value, self.config.cache_ttl_like_status_secs)
            .await;

        Ok(LikeStatusResponse {
            liked: liked_at.is_some(),
            liked_at,
        })
    }

    /// Get a user's liked items with cursor-based pagination.
    /// First page (cursor=None) is cached; subsequent pages bypass cache.
    pub(crate) async fn get_user_likes(
        &self,
        user_id: Uuid,
        content_type_filter: Option<&str>,
        cursor: Option<&str>,
        limit: i64,
    ) -> Result<PaginatedResponse<UserLikeItem>, AppError> {
        let limit = limit.clamp(1, 100);

        // Only cache first page (no cursor)
        if cursor.is_none() {
            let filter_key = content_type_filter.unwrap_or("_");
            let cache_key = format!("ul:{user_id}:{filter_key}");

            // Check cache
            if let Some(cached) = self.cache.get(&cache_key).await
                && let Ok(page) = serde_json::from_str::<CachedUserLikePage>(&cached)
            {
                return Ok(build_paginated_response(&page, limit));
            }

            // Cache miss: query with max limit (100) to store a reusable full page
            let max_limit: i64 = 100;
            let rows = repositories::get_user_likes(
                &self.db.reader,
                user_id,
                content_type_filter,
                None,
                None,
                max_limit,
            )
            .await
            .map_err(map_db_error)?;

            let has_more = rows.len() as i64 > max_limit;
            let take_n = (max_limit as usize).min(rows.len());
            let page = CachedUserLikePage {
                rows: rows[..take_n].to_vec(),
                has_more,
            };

            // Cache the full first page
            if let Ok(json) = serde_json::to_string(&page) {
                self.cache
                    .set(&cache_key, &json, self.config.cache_ttl_user_likes_secs)
                    .await;
            }

            return Ok(build_paginated_response(&page, limit));
        }

        // Subsequent pages: decode cursor, query DB directly
        let (cursor_ts, cursor_id) = if let Some(cursor_str) = cursor {
            let cursor = shared::cursor::Cursor::decode(cursor_str)
                .map_err(|_| AppError::InvalidCursor("Malformed cursor".to_string()))?;
            (Some(cursor.timestamp), Some(cursor.id))
        } else {
            (None, None)
        };

        let rows = repositories::get_user_likes(
            &self.db.reader,
            user_id,
            content_type_filter,
            cursor_ts,
            cursor_id,
            limit,
        )
        .await
        .map_err(map_db_error)?;

        let has_more = rows.len() as i64 > limit;
        let take_n = (limit as usize).min(rows.len());
        let items: Vec<_> = rows
            .iter()
            .take(take_n)
            .map(|row| UserLikeItem {
                content_type: row.content_type.clone(),
                content_id: row.content_id,
                liked_at: row.created_at,
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

    pub(crate) async fn batch_counts(
        &self,
        items: &[(String, Uuid)],
    ) -> Result<Vec<BatchCountResult>, AppError> {
        self.count_cache.batch_counts(items).await
    }

    /// Batch get like statuses with L1 → Redis → DB fallback.
    /// Uses individual `ls:` keys so single-status reads share the cache.
    pub(crate) async fn batch_statuses(
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

        // Build per-item cache keys
        let cache_keys: Vec<String> = items
            .iter()
            .map(|(ct, cid)| status_cache_key(user_id, ct, *cid))
            .collect();

        let cached_values = self.cache.mget(&cache_keys).await;

        let mut results = Vec::with_capacity(items.len());
        let mut missing_indices: Vec<usize> = Vec::new();

        for (idx, (content_type, content_id)) in items.iter().enumerate() {
            if let Some(Some(value)) = cached_values.get(idx) {
                let status = parse_status_cache_value(value);
                results.push(BatchStatusResult {
                    content_type: content_type.clone(),
                    content_id: *content_id,
                    liked: status.liked,
                    liked_at: status.liked_at,
                });
            } else {
                // Placeholder — will be filled from DB
                missing_indices.push(idx);
                results.push(BatchStatusResult {
                    content_type: content_type.clone(),
                    content_id: *content_id,
                    liked: false,
                    liked_at: None,
                });
            }
        }

        if missing_indices.is_empty() {
            return Ok(results);
        }

        // Build items slice for DB query (only misses)
        let missing_items: Vec<(String, Uuid)> = missing_indices
            .iter()
            .map(|&idx| items[idx].clone())
            .collect();

        let db_rows = repositories::batch_get_statuses(&self.db.reader, user_id, &missing_items)
            .await
            .map_err(map_db_error)?;

        // Index DB results by (content_type, content_id)
        let db_map: std::collections::HashMap<(String, Uuid), Option<DateTime<Utc>>> = db_rows
            .into_iter()
            .map(|(ct, cid, ts)| ((ct, cid), ts))
            .collect();

        // Fill results and build cache entries for set_many
        let mut cache_entries: Vec<(String, String)> = Vec::with_capacity(missing_indices.len());

        for &idx in &missing_indices {
            let (content_type, content_id) = &items[idx];
            let liked_at = db_map
                .get(&(content_type.clone(), *content_id))
                .and_then(|ts| *ts);

            results[idx].liked = liked_at.is_some();
            results[idx].liked_at = liked_at;

            let value = match liked_at {
                Some(ts) => ts.to_rfc3339(),
                None => String::new(),
            };
            cache_entries.push((cache_keys[idx].clone(), value));
        }

        // Cache all DB results in one pipeline
        let ttl = self.config.cache_ttl_like_status_secs;
        let set_entries: Vec<(&str, &str, u64)> = cache_entries
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str(), ttl))
            .collect();
        self.cache.set_many(&set_entries).await;

        Ok(results)
    }

    /// Get leaderboard, with caching for filtered queries.
    /// Unfiltered: existing ZSET path. Filtered: string cache (L1 → Redis → DB).
    pub(crate) async fn get_leaderboard(
        &self,
        content_type: Option<&str>,
        window: TimeWindow,
        limit: i64,
    ) -> Result<TopLikedResponse, AppError> {
        let limit = limit.clamp(1, 50);

        if content_type.is_none() {
            // Unfiltered: use existing ZSET cache path
            let key = format!("lb:{}", window.as_str());
            let cached = self
                .cache
                .zrevrange_with_scores(&key, 0, (limit - 1) as isize)
                .await;

            if !cached.is_empty() {
                let items = cached
                    .iter()
                    .filter_map(|(member, score)| parse_leaderboard_member(member, *score))
                    .collect();

                return Ok(TopLikedResponse {
                    window: window.as_str().to_string(),
                    content_type: None,
                    items,
                });
            }
        } else if let Some(ct) = content_type {
            // Filtered: check string cache (L1 → Redis)
            let cache_key = format!("lbf:{}:{ct}:{limit}", window.as_str());

            if let Some(cached) = self.cache.get(&cache_key).await
                && let Ok(items) = serde_json::from_str::<Vec<TopLikedItem>>(&cached)
            {
                return Ok(TopLikedResponse {
                    window: window.as_str().to_string(),
                    content_type: Some(ct.to_string()),
                    items,
                });
            }

            // Cache miss: query DB
            let since = window
                .duration_secs()
                .map(|secs| Utc::now() - Duration::seconds(secs));

            let rows = repositories::get_leaderboard(&self.db.reader, Some(ct), since, limit)
                .await
                .map_err(map_db_error)?;

            let items: Vec<TopLikedItem> = rows
                .into_iter()
                .map(|(content_type, content_id, count)| TopLikedItem {
                    content_type,
                    content_id,
                    count,
                })
                .collect();

            // Cache the result
            if let Ok(json) = serde_json::to_string(&items) {
                self.cache
                    .set(
                        &cache_key,
                        &json,
                        self.config.cache_ttl_leaderboard_filtered_secs,
                    )
                    .await;
            }

            return Ok(TopLikedResponse {
                window: window.as_str().to_string(),
                content_type: Some(ct.to_string()),
                items,
            });
        }

        // Fallback: unfiltered ZSET miss or no content_type — query DB directly
        let since = window
            .duration_secs()
            .map(|secs| Utc::now() - Duration::seconds(secs));

        let rows = repositories::get_leaderboard(&self.db.reader, content_type, since, limit)
            .await
            .map_err(map_db_error)?;

        let items = rows
            .into_iter()
            .map(|(content_type, content_id, count)| TopLikedItem {
                content_type,
                content_id,
                count,
            })
            .collect();

        Ok(TopLikedResponse {
            window: window.as_str().to_string(),
            content_type: content_type.map(ToOwned::to_owned),
            items,
        })
    }
}

/// Parse a cached status value: non-empty = liked (RFC3339 timestamp), empty = not liked.
fn parse_status_cache_value(value: &str) -> LikeStatusResponse {
    if value.is_empty() {
        LikeStatusResponse {
            liked: false,
            liked_at: None,
        }
    } else {
        match DateTime::parse_from_rfc3339(value) {
            Ok(ts) => LikeStatusResponse {
                liked: true,
                liked_at: Some(ts.with_timezone(&Utc)),
            },
            Err(_) => {
                // Corrupt cache entry — treat as miss
                tracing::warn!(value, "Invalid RFC3339 in like status cache");
                LikeStatusResponse {
                    liked: false,
                    liked_at: None,
                }
            }
        }
    }
}

/// Build cache key for a single like status.
fn status_cache_key(user_id: Uuid, content_type: &str, content_id: Uuid) -> String {
    format!("ls:{user_id}:{content_type}:{content_id}")
}

/// Build a PaginatedResponse from a cached full first page, sliced to `limit`.
fn build_paginated_response(
    page: &CachedUserLikePage,
    limit: i64,
) -> PaginatedResponse<UserLikeItem> {
    let limit = limit as usize;
    let take_n = limit.min(page.rows.len());

    let items: Vec<UserLikeItem> = page.rows[..take_n]
        .iter()
        .map(|row| UserLikeItem {
            content_type: row.content_type.clone(),
            content_id: row.content_id,
            liked_at: row.created_at,
        })
        .collect();

    let has_more = if page.rows.len() > limit {
        true
    } else if page.rows.len() == limit {
        page.has_more
    } else {
        false
    };

    let next_cursor = if has_more {
        items.last().map(|last| {
            let last_row = &page.rows[items.len() - 1];
            shared::cursor::Cursor::new(last.liked_at, last_row.id).encode()
        })
    } else {
        None
    };

    PaginatedResponse {
        items,
        next_cursor,
        has_more,
    }
}

fn parse_leaderboard_member(member: &str, score: f64) -> Option<TopLikedItem> {
    let colon_pos = member.find(':')?;
    let content_type = &member[..colon_pos];
    let content_id = &member[colon_pos + 1..];

    match Uuid::parse_str(content_id) {
        Ok(content_id) => Some(TopLikedItem {
            content_type: content_type.to_string(),
            content_id,
            count: score as i64,
        }),
        Err(error) => {
            tracing::warn!(
                member,
                error = %error,
                "Invalid UUID in leaderboard cache member"
            );
            None
        }
    }
}
