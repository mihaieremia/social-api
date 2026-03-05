//! Shared test infrastructure for in-process axum tests.
//!
//! `TestApp::new()` spins up real Postgres and Redis via testcontainers,
//! runs all migrations, then builds the full Axum router with stub
//! implementations of the external dependencies (token validator, content
//! validator).

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt as _;
use metrics_exporter_prometheus::PrometheusBuilder;
use shared::errors::AppError;
use shared::types::AuthenticatedUser;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;
use uuid::Uuid;

use social_api::cache::manager::{CacheManager, create_pool};
use social_api::clients::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use social_api::clients::content_client::ContentValidator;
use social_api::clients::profile_client::TokenValidator;
use social_api::config::Config;
use social_api::db::DbPools;
use social_api::server::build_router;
use social_api::services::like_service::LikeService;
use social_api::state::AppState;

/// A fully wired axum router backed by real Postgres + Redis containers.
pub struct TestApp {
    pub router: axum::Router,
}

impl TestApp {
    pub async fn new() -> Self {
        let pg = testcontainers_modules::postgres::Postgres::default()
            .start()
            .await
            .expect("failed to start postgres container");
        let pg_port = pg.get_host_port_ipv4(5432).await.unwrap();
        let db_url = format!("postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres");

        let redis = testcontainers_modules::redis::Redis::default()
            .start()
            .await
            .expect("failed to start redis container");
        let redis_port = redis.get_host_port_ipv4(6379).await.unwrap();
        let redis_url = format!("redis://127.0.0.1:{redis_port}");

        let mut config = Config::new_for_test();
        config.database_url = db_url.clone();
        config.read_database_url = db_url;
        config.redis_url = redis_url;

        let db = DbPools::from_config(&config).await.expect("db pools");
        sqlx::migrate!("../../migrations")
            .run(&db.writer)
            .await
            .expect("migrations");

        let redis_pool = create_pool(&config).await.expect("redis pool");
        let cache = CacheManager::new(redis_pool);

        let content_breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 5,
            recovery_timeout: std::time::Duration::from_secs(30),
            success_threshold: 2,
            service_name: "content_api_test".to_string(),
            rate_window: std::time::Duration::from_secs(30),
            failure_rate_threshold: 0.5,
            min_calls_for_rate: 10,
        }));

        let like_service = LikeService::new_with_validator(
            db.clone(),
            cache.clone(),
            Arc::new(TestContentValidator),
            config.clone(),
            content_breaker,
        );

        let shutdown_token = CancellationToken::new();

        let state = AppState::new_for_test(
            db.clone(),
            cache.clone(),
            config,
            shutdown_token,
            Box::new(TestTokenValidator),
            like_service,
        );

        let metrics_handle = PrometheusBuilder::new()
            .build_recorder()
            .handle();

        // Keep containers alive for the full test duration by leaking them.
        // This is acceptable in tests — the OS reclaims memory on exit.
        Box::leak(Box::new(pg));
        Box::leak(Box::new(redis));

        let router = build_router(state, metrics_handle);
        TestApp { router }
    }

    /// Send a single request through the router using tower's `oneshot`.
    pub async fn request(&self, req: Request<Body>) -> axum::response::Response {
        self.router
            .clone()
            .oneshot(req)
            .await
            .expect("router oneshot failed")
    }
}

/// Stub token validator: the Bearer token must be a plain UUID.
/// The UUID becomes the authenticated user_id directly.
pub struct TestTokenValidator;

#[async_trait::async_trait]
impl TokenValidator for TestTokenValidator {
    async fn validate(&self, token: &str) -> Result<AuthenticatedUser, AppError> {
        let user_id = Uuid::parse_str(token)
            .map_err(|_| AppError::Unauthorized("token must be a UUID in tests".into()))?;
        Ok(AuthenticatedUser {
            user_id,
            display_name: "Test User".to_string(),
        })
    }
}

/// Stub content validator: all content is considered valid.
pub struct TestContentValidator;

#[async_trait::async_trait]
impl ContentValidator for TestContentValidator {
    async fn validate(
        &self,
        _content_type: &str,
        _content_id: Uuid,
    ) -> Result<bool, AppError> {
        Ok(true)
    }
}

// ── Request builder helpers ──────────────────────────────────────────────────

pub fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

pub fn authed_json_request(
    method: &str,
    uri: &str,
    user_id: Uuid,
    body: serde_json::Value,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {user_id}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

pub fn get_request(uri: &str, user_id: Option<Uuid>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(id) = user_id {
        builder = builder.header("authorization", format!("Bearer {id}"));
    }
    builder.body(Body::empty()).unwrap()
}

/// Collect the full response body and deserialize it as JSON.
pub async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}
