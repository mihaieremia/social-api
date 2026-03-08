use std::sync::Arc;

use chrono::Utc;
use shared::errors::AppError;
use shared::types::{LikeActionResponse, LikeEvent};
use uuid::Uuid;

use crate::clients::circuit_breaker::CircuitBreaker;
use crate::clients::content_client::ContentValidator;
use crate::db::DbPools;
use crate::repositories;
use crate::services::like_count_cache::{LikeCountCache, map_db_error};
use crate::services::like_events::LikeEventPublisher;

pub(crate) struct LikeCommandService {
    db: DbPools,
    content_validator: Arc<dyn ContentValidator>,
    content_breaker: Arc<CircuitBreaker>,
    count_cache: Arc<LikeCountCache>,
    event_publisher: LikeEventPublisher,
}

impl LikeCommandService {
    pub(crate) fn new(
        db: DbPools,
        content_validator: Arc<dyn ContentValidator>,
        content_breaker: Arc<CircuitBreaker>,
        count_cache: Arc<LikeCountCache>,
        event_publisher: LikeEventPublisher,
    ) -> Self {
        Self {
            db,
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

    async fn validate_content(&self, content_type: &str, content_id: Uuid) -> Result<(), AppError> {
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
            Err(error) => {
                self.content_breaker.record_failure();
                Err(error)
            }
        }
    }
}
