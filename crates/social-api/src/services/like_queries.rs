use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use shared::errors::AppError;
use shared::types::{
    BatchCountResult, BatchStatusResult, LikeCountResponse, LikeStatusResponse, PaginatedResponse,
    TimeWindow, TopLikedItem, TopLikedResponse, UserLikeItem,
};
use uuid::Uuid;

use crate::cache::CacheManager;
use crate::db::DbPools;
use crate::repositories;
use crate::services::like_count_cache::{LikeCountCache, map_db_error};

pub(crate) struct LikeQueryService {
    db: DbPools,
    cache: CacheManager,
    count_cache: Arc<LikeCountCache>,
}

impl LikeQueryService {
    pub(crate) fn new(db: DbPools, cache: CacheManager, count_cache: Arc<LikeCountCache>) -> Self {
        Self {
            db,
            cache,
            count_cache,
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

    pub(crate) async fn get_status(
        &self,
        user_id: Uuid,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeStatusResponse, AppError> {
        let liked_at =
            repositories::get_like_status(&self.db.reader, user_id, content_type, content_id)
                .await
                .map_err(map_db_error)?;

        Ok(LikeStatusResponse {
            liked: liked_at.is_some(),
            liked_at,
        })
    }

    pub(crate) async fn get_user_likes(
        &self,
        user_id: Uuid,
        content_type_filter: Option<&str>,
        cursor: Option<&str>,
        limit: i64,
    ) -> Result<PaginatedResponse<UserLikeItem>, AppError> {
        let limit = limit.clamp(1, 100);

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

        let rows = repositories::batch_get_statuses(&self.db.reader, user_id, items)
            .await
            .map_err(map_db_error)?;

        let mut result_map: std::collections::HashMap<(String, Uuid), Option<DateTime<Utc>>> = rows
            .into_iter()
            .map(|(content_type, content_id, liked_at)| ((content_type, content_id), liked_at))
            .collect();

        Ok(items
            .iter()
            .map(|(content_type, content_id)| {
                let liked_at = result_map
                    .remove(&(content_type.clone(), *content_id))
                    .flatten();
                BatchStatusResult {
                    content_type: content_type.clone(),
                    content_id: *content_id,
                    liked: liked_at.is_some(),
                    liked_at,
                }
            })
            .collect())
    }

    pub(crate) async fn get_leaderboard(
        &self,
        content_type: Option<&str>,
        window: TimeWindow,
        limit: i64,
    ) -> Result<TopLikedResponse, AppError> {
        let limit = limit.clamp(1, 50);

        if content_type.is_none() {
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
        }

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
