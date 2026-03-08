use std::sync::Arc;

use chrono::Utc;
use shared::errors::AppError;
use shared::types::{LikeActionResponse, LikeEvent};
use uuid::Uuid;

use crate::cache::CacheManager;
use crate::clients::circuit_breaker::CircuitBreaker;
use crate::clients::content_client::{ContentValidator, ValidationOutcome};
use crate::config::Config;
use crate::db::DbPools;
use crate::repositories;
use crate::services::like_count_cache::{LikeCountCache, map_db_error};
use crate::services::like_events::LikeEventPublisher;

pub(crate) struct LikeCommandService {
    db: DbPools,
    cache: CacheManager,
    config: Config,
    content_validator: Arc<dyn ContentValidator>,
    content_breaker: Arc<CircuitBreaker>,
    count_cache: Arc<LikeCountCache>,
    event_publisher: LikeEventPublisher,
}

impl LikeCommandService {
    pub(crate) fn new(
        db: DbPools,
        cache: CacheManager,
        config: Config,
        content_validator: Arc<dyn ContentValidator>,
        content_breaker: Arc<CircuitBreaker>,
        count_cache: Arc<LikeCountCache>,
        event_publisher: LikeEventPublisher,
    ) -> Self {
        Self {
            db,
            cache,
            config,
            content_validator,
            content_breaker,
            count_cache,
            event_publisher,
        }
    }

    pub(crate) async fn like(
        &self,
        user_id: Uuid,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeActionResponse, AppError> {
        self.validate_content(content_type, content_id).await?;

        let result = repositories::insert_like(&self.db.writer, user_id, content_type, content_id)
            .await
            .map_err(map_db_error)?;

        match result {
            repositories::InsertLikeResult::Inserted { row, count } => {
                self.count_cache
                    .write_count(content_type, content_id, count)
                    .await;
                self.update_status_cache(user_id, content_type, content_id, Some(row.created_at))
                    .await;
                self.invalidate_user_likes(user_id, content_type).await;
                self.event_publisher
                    .publish(
                        LikeEvent::Liked {
                            user_id,
                            count,
                            timestamp: row.created_at,
                        },
                        content_type,
                        content_id,
                    )
                    .await;
                LikeEventPublisher::record_metric("like", content_type);
                Ok(LikeActionResponse {
                    liked: true,
                    already_existed: Some(false),
                    was_liked: None,
                    count,
                    liked_at: Some(row.created_at),
                })
            }
            repositories::InsertLikeResult::AlreadyExisted { row, count } => {
                self.count_cache
                    .write_count(content_type, content_id, count)
                    .await;
                self.update_status_cache(user_id, content_type, content_id, Some(row.created_at))
                    .await;
                Ok(LikeActionResponse {
                    liked: true,
                    already_existed: Some(true),
                    was_liked: None,
                    count,
                    liked_at: Some(row.created_at),
                })
            }
            repositories::InsertLikeResult::ConcurrentlyRemoved { count } => {
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

    pub(crate) async fn unlike(
        &self,
        user_id: Uuid,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeActionResponse, AppError> {
        self.validate_content(content_type, content_id).await?;

        let result = repositories::delete_like(&self.db.writer, user_id, content_type, content_id)
            .await
            .map_err(map_db_error)?;

        let count = result.count;
        self.count_cache
            .write_count(content_type, content_id, count)
            .await;
        self.update_status_cache(user_id, content_type, content_id, None)
            .await;
        self.invalidate_user_likes(user_id, content_type).await;

        if result.was_liked {
            self.event_publisher
                .publish(
                    LikeEvent::Unliked {
                        user_id,
                        count,
                        timestamp: Utc::now(),
                    },
                    content_type,
                    content_id,
                )
                .await;
            LikeEventPublisher::record_metric("unlike", content_type);
        }

        Ok(LikeActionResponse {
            liked: false,
            already_existed: None,
            was_liked: Some(result.was_liked),
            count,
            liked_at: None,
        })
    }

    /// Write-through: update like status cache immediately after mutation.
    /// `liked_at = Some(ts)` for like, `None` for unlike (stored as "").
    async fn update_status_cache(
        &self,
        user_id: Uuid,
        content_type: &str,
        content_id: Uuid,
        liked_at: Option<chrono::DateTime<Utc>>,
    ) {
        let key = format!("ls:{user_id}:{content_type}:{content_id}");
        let value = match liked_at {
            Some(ts) => ts.to_rfc3339(),
            None => String::new(),
        };
        self.cache
            .set(&key, &value, self.config.cache_ttl_like_status_secs)
            .await;
    }

    /// Invalidate user likes first-page cache after a like/unlike mutation.
    /// L1: prefix invalidation. Redis: explicit DEL on known keys.
    async fn invalidate_user_likes(&self, user_id: Uuid, content_type: &str) {
        // L1: invalidate all entries for this user
        let prefix = format!("ul:{user_id}:");
        self.cache.invalidate_l1_prefix(&prefix);

        // Redis: delete the two known keys (no-filter + this content_type filter)
        let key_all = format!("ul:{user_id}:_");
        let key_typed = format!("ul:{user_id}:{content_type}");
        self.cache.del_many(&[&key_all, &key_typed]).await;
    }

    async fn validate_content(&self, content_type: &str, content_id: Uuid) -> Result<(), AppError> {
        if !self.content_breaker.allow_request() {
            return Err(AppError::DependencyUnavailable("content_api".to_string()));
        }

        match self
            .content_validator
            .validate(content_type, content_id)
            .await
        {
            Ok(ValidationOutcome::Remote(valid)) => {
                // Real HTTP/gRPC call succeeded — signal the circuit breaker.
                self.content_breaker.record_success();
                if !valid {
                    return Err(AppError::ContentNotFound {
                        content_type: content_type.to_string(),
                        content_id: content_id.to_string(),
                    });
                }
                Ok(())
            }
            Ok(ValidationOutcome::Cached(valid)) => {
                // Cache hit — no remote call was made, so do NOT signal the breaker.
                // Signaling on cache hits would prevent the breaker from opening when
                // the upstream is down but cached results are still being served.
                if !valid {
                    return Err(AppError::ContentNotFound {
                        content_type: content_type.to_string(),
                        content_id: content_id.to_string(),
                    });
                }
                Ok(())
            }
            Err(error) => {
                self.content_breaker.record_failure();
                Err(error)
            }
        }
    }
}
