use std::sync::Arc;

use shared::errors::AppError;
use shared::types::{
    BatchCountResult, BatchStatusResult, LikeActionResponse, LikeCountResponse, LikeStatusResponse,
    PaginatedResponse, TimeWindow, TopLikedResponse, UserLikeItem,
};
use uuid::Uuid;

use crate::cache::CacheManager;
use crate::clients::circuit_breaker::CircuitBreaker;
use crate::clients::content_client::{ContentValidator, HttpContentValidator};
use crate::config::Config;
use crate::db::DbPools;
use crate::services::like_commands::LikeCommandService;
use crate::services::like_count_cache::LikeCountCache;
use crate::services::like_events::LikeEventPublisher;
use crate::services::like_queries::LikeQueryService;

/// Thin facade for like-related business operations.
///
/// The public API stays stable for handlers and gRPC services while command,
/// query, cache, and event responsibilities live in focused helper services.
pub struct LikeService {
    commands: LikeCommandService,
    queries: LikeQueryService,
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
        Self::new_with_validator(db, cache, content_validator, config, content_breaker)
    }

    pub fn new_with_validator(
        db: DbPools,
        cache: CacheManager,
        content_validator: Arc<dyn ContentValidator>,
        config: Config,
        content_breaker: Arc<CircuitBreaker>,
    ) -> Self {
        let count_cache = Arc::new(LikeCountCache::new(
            db.clone(),
            cache.clone(),
            config.clone(),
        ));
        let event_publisher = LikeEventPublisher::new(cache.clone());

        let commands = LikeCommandService::new(
            db.clone(),
            cache.clone(),
            config.clone(),
            content_validator,
            content_breaker.clone(),
            count_cache.clone(),
            event_publisher,
        );
        let queries = LikeQueryService::new(db, cache.clone(), count_cache, config);

        Self { commands, queries }
    }

    pub async fn like(
        &self,
        user_id: Uuid,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeActionResponse, AppError> {
        self.commands.like(user_id, content_type, content_id).await
    }

    pub async fn unlike(
        &self,
        user_id: Uuid,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeActionResponse, AppError> {
        self.commands
            .unlike(user_id, content_type, content_id)
            .await
    }

    pub async fn get_count(
        &self,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeCountResponse, AppError> {
        self.queries.get_count(content_type, content_id).await
    }

    pub async fn get_status(
        &self,
        user_id: Uuid,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeStatusResponse, AppError> {
        self.queries
            .get_status(user_id, content_type, content_id)
            .await
    }

    pub async fn get_user_likes(
        &self,
        user_id: Uuid,
        content_type_filter: Option<&str>,
        cursor: Option<&str>,
        limit: i64,
    ) -> Result<PaginatedResponse<UserLikeItem>, AppError> {
        self.queries
            .get_user_likes(user_id, content_type_filter, cursor, limit)
            .await
    }

    pub async fn batch_counts(
        &self,
        items: &[(String, Uuid)],
    ) -> Result<Vec<BatchCountResult>, AppError> {
        self.queries.batch_counts(items).await
    }

    pub async fn batch_statuses(
        &self,
        user_id: Uuid,
        items: &[(String, Uuid)],
    ) -> Result<Vec<BatchStatusResult>, AppError> {
        self.queries.batch_statuses(user_id, items).await
    }

    pub async fn get_leaderboard(
        &self,
        content_type: Option<&str>,
        window: TimeWindow,
        limit: i64,
    ) -> Result<TopLikedResponse, AppError> {
        self.queries
            .get_leaderboard(content_type, window, limit)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::content_client::ValidationOutcome;
    use std::sync::Arc;
    use uuid::Uuid;

    struct TestHarness {
        _scope: crate::test_containers::TestScope,
        svc: LikeService,
        cache: CacheManager,
        content_breaker: Arc<CircuitBreaker>,
    }

    struct AlwaysValidContent;

    #[async_trait::async_trait]
    impl ContentValidator for AlwaysValidContent {
        async fn validate(&self, _ct: &str, _id: Uuid) -> Result<ValidationOutcome, AppError> {
            Ok(ValidationOutcome::Remote(true))
        }
    }

    struct AlwaysInvalidContent;

    #[async_trait::async_trait]
    impl ContentValidator for AlwaysInvalidContent {
        async fn validate(&self, _ct: &str, _id: Uuid) -> Result<ValidationOutcome, AppError> {
            Ok(ValidationOutcome::Remote(false))
        }
    }

    async fn make_service(validator: Arc<dyn ContentValidator>) -> TestHarness {
        let scope = crate::test_containers::isolated_scope().await;

        let mut config = crate::config::Config::new_for_test();
        config.database_url = scope.database_url.clone();
        config.read_database_url = scope.database_url.clone();
        config.redis_url = scope.redis_url.clone();

        let db = crate::db::DbPools::from_config(&config)
            .await
            .expect("db pools");

        let redis_pool = crate::cache::create_pool(&config)
            .await
            .expect("redis pool");
        let cache = crate::cache::CacheManager::new(redis_pool, &config);

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

        let svc = LikeService::new_with_validator(
            db,
            cache.clone(),
            validator,
            config,
            content_breaker.clone(),
        );

        TestHarness {
            _scope: scope,
            svc,
            cache,
            content_breaker,
        }
    }

    #[tokio::test]
    async fn test_like_new_content_increments_count() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let resp = svc.like(user_id, "post", content_id).await.unwrap();
        assert!(resp.liked);
        assert_eq!(resp.count, 1);

        let count_resp = svc.get_count("post", content_id).await.unwrap();
        assert_eq!(count_resp.count, 1);
    }

    #[tokio::test]
    async fn test_like_duplicate_is_idempotent() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        svc.like(user_id, "post", content_id).await.unwrap();
        let resp = svc.like(user_id, "post", content_id).await.unwrap();

        assert!(resp.liked);
        assert_eq!(resp.already_existed, Some(true));

        let count_resp = svc.get_count("post", content_id).await.unwrap();
        assert_eq!(count_resp.count, 1);
    }

    #[tokio::test]
    async fn test_like_invalid_content_rolls_back() {
        let harness = make_service(Arc::new(AlwaysInvalidContent)).await;
        let svc = harness.svc;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let err = svc.like(user_id, "post", content_id).await.unwrap_err();
        assert!(
            matches!(err, AppError::ContentNotFound { .. }),
            "expected ContentNotFound, got: {err:?}"
        );

        let count_resp = svc.get_count("post", content_id).await.unwrap();
        assert_eq!(count_resp.count, 0);
    }

    #[tokio::test]
    async fn test_unlike_decrements_count() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        svc.like(user_id, "post", content_id).await.unwrap();

        let resp = svc.unlike(user_id, "post", content_id).await.unwrap();
        assert!(!resp.liked);
        assert_eq!(resp.was_liked, Some(true));

        let count_resp = svc.get_count("post", content_id).await.unwrap();
        assert_eq!(count_resp.count, 0);
    }

    #[tokio::test]
    async fn test_unlike_not_liked_is_idempotent() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let resp = svc.unlike(user_id, "post", content_id).await.unwrap();
        assert!(!resp.liked);
        assert_eq!(resp.was_liked, Some(false));
    }

    #[tokio::test]
    async fn test_get_count_cache_hit() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        svc.like(user_id, "post", content_id).await.unwrap();

        let first = svc.get_count("post", content_id).await.unwrap();
        let second = svc.get_count("post", content_id).await.unwrap();

        assert_eq!(first.count, second.count);
        assert_eq!(first.count, 1);
    }

    #[tokio::test]
    async fn test_batch_counts_returns_correct_values() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
        let user_id = Uuid::new_v4();
        let content_id_a = Uuid::new_v4();
        let content_id_b = Uuid::new_v4();

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

    #[tokio::test]
    async fn test_batch_counts_too_large_returns_error() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;

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

    #[tokio::test]
    async fn test_get_leaderboard_returns_ok() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;

        let resp = svc
            .get_leaderboard(None, TimeWindow::All, 10)
            .await
            .unwrap();

        assert_eq!(resp.window, "all");
        assert!(resp.items.len() <= 10);
    }

    #[tokio::test]
    async fn test_get_user_likes_pagination() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
        let user_id = Uuid::new_v4();

        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        for &cid in &ids {
            svc.like(user_id, "post", cid).await.unwrap();
        }

        let page1 = svc.get_user_likes(user_id, None, None, 2).await.unwrap();
        assert_eq!(page1.items.len(), 2);
        assert!(page1.has_more);
        assert!(page1.next_cursor.is_some());

        let cursor = page1.next_cursor.as_deref();
        let page2 = svc.get_user_likes(user_id, None, cursor, 2).await.unwrap();
        assert_eq!(page2.items.len(), 1);
        assert!(!page2.has_more);
    }

    #[tokio::test]
    async fn test_batch_statuses_returns_correct_status() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
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

    #[tokio::test]
    async fn test_stampede_coalescing() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = Arc::new(harness.svc);
        let content_id = Uuid::new_v4();

        let handles: Vec<_> = (0..5)
            .map(|_| {
                let svc = Arc::clone(&svc);
                tokio::spawn(async move { svc.get_count("post", content_id).await })
            })
            .collect();

        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.expect("task did not panic"));
        }

        for result in results {
            assert_eq!(result.unwrap().count, 0);
        }
    }

    #[tokio::test]
    async fn test_get_leaderboard_with_content_type_filter() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
        let cache = &harness.cache;
        let post_id = Uuid::new_v4();

        // Simulate what the refresh task does: pre-populate the filtered
        // leaderboard cache key so the query service finds a cache hit.
        let items = vec![shared::types::TopLikedItem {
            content_type: "post".to_string(),
            content_id: post_id,
            count: 1,
        }];
        let json = serde_json::to_string(&items).unwrap();
        // The query clamps limit to 1..=50, so limit=10 stays 10.
        // Cache key format: lbf:{window}:{type}:{limit}
        cache.set("lbf:all:post:10", &json, 60).await;

        let resp = svc
            .get_leaderboard(Some("post"), TimeWindow::All, 10)
            .await
            .unwrap();

        assert_eq!(resp.window, "all");
        assert_eq!(resp.content_type, Some("post".to_string()));
        assert!(!resp.items.is_empty(), "expected at least one item");
        assert_eq!(resp.items[0].count, 1);
        assert_eq!(resp.items[0].content_id, post_id);
    }

    #[tokio::test]
    async fn test_get_leaderboard_from_cache() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
        let cache = harness.cache;
        let content_id = Uuid::new_v4();

        let key = "lb:all";
        let member = format!("post:{content_id}");
        cache.replace_sorted_set(key, &[(member, 5.0)]).await;

        let resp = svc
            .get_leaderboard(None, TimeWindow::All, 10)
            .await
            .unwrap();

        assert_eq!(resp.window, "all");
        assert!(!resp.items.is_empty());
    }

    #[tokio::test]
    async fn test_get_status_liked_and_not_liked() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
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

    #[tokio::test]
    async fn test_get_user_likes_with_content_type_filter() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
        let user_id = Uuid::new_v4();

        let post_id = Uuid::new_v4();
        let bonus_id = Uuid::new_v4();

        svc.like(user_id, "post", post_id).await.unwrap();
        svc.like(user_id, "bonus_hunter", bonus_id).await.unwrap();

        let page = svc
            .get_user_likes(user_id, Some("post"), None, 10)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].content_type, "post");
        assert!(!page.has_more);
    }

    #[tokio::test]
    async fn test_get_user_likes_bad_cursor_returns_error() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
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

    #[tokio::test]
    async fn test_batch_statuses_too_large_returns_error() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
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

    #[tokio::test]
    async fn test_circuit_breaker_open_rejects_like() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
        let content_breaker = harness.content_breaker;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        for _ in 0..105 {
            content_breaker.record_failure();
        }

        let err = svc.like(user_id, "post", content_id).await.unwrap_err();
        assert!(
            matches!(err, AppError::DependencyUnavailable(_)),
            "expected DependencyUnavailable, got: {err:?}"
        );
    }

    struct AlwaysErrorContent;

    #[async_trait::async_trait]
    impl ContentValidator for AlwaysErrorContent {
        async fn validate(&self, _ct: &str, _id: Uuid) -> Result<ValidationOutcome, AppError> {
            Err(AppError::DependencyUnavailable("content_api".to_string()))
        }
    }

    #[tokio::test]
    async fn test_content_validator_error_rolls_back_and_records_failure() {
        let harness = make_service(Arc::new(AlwaysErrorContent)).await;
        let svc = harness.svc;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let err = svc.like(user_id, "post", content_id).await.unwrap_err();
        assert!(
            matches!(err, AppError::DependencyUnavailable(_)),
            "expected DependencyUnavailable, got: {err:?}"
        );

        let count_resp = svc.get_count("post", content_id).await.unwrap();
        assert_eq!(count_resp.count, 0);
    }

    #[tokio::test]
    async fn test_get_leaderboard_different_time_windows() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;

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

    #[tokio::test]
    async fn test_batch_counts_all_cache_hits() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        svc.like(user_id, "post", content_id).await.unwrap();
        svc.get_count("post", content_id).await.unwrap();

        let items = vec![("post".to_string(), content_id)];
        let results = svc.batch_counts(&items).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].count, 1);
    }

    #[tokio::test]
    async fn test_unlike_not_liked_get_count_inner_path() {
        let harness = make_service(Arc::new(AlwaysValidContent)).await;
        let svc = harness.svc;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let resp = svc.unlike(user_id, "post", content_id).await.unwrap();
        assert_eq!(resp.was_liked, Some(false));
        assert_eq!(resp.count, 0);
    }
}
