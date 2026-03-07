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
use testcontainers::runners::AsyncRunner;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;
use uuid::Uuid;

use social_api::cache::manager::{CacheManager, create_pool};
use social_api::clients::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use social_api::clients::content_client::ContentValidator;
use social_api::clients::profile_client::TokenValidator;
use social_api::config::Config;
use social_api::db::DbPools;
use social_api::server::{build_grpc_server, build_router};
use social_api::services::like_service::LikeService;
use social_api::state::AppState;
use tokio::net::TcpListener;

// ── Shared containers for integration tests ──────────────────────────────────

struct SharedContainers {
    _pg: testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    _redis: testcontainers::ContainerAsync<testcontainers_modules::redis::Redis>,
    db_url: String,
    redis_url: String,
}

static CONTAINERS: OnceCell<SharedContainers> = OnceCell::const_new();

async fn shared_containers() -> &'static SharedContainers {
    CONTAINERS
        .get_or_init(|| async {
            let pg = testcontainers_modules::postgres::Postgres::default()
                .start()
                .await
                .expect("postgres container");
            let pg_port = pg.get_host_port_ipv4(5432).await.unwrap();
            let db_url = format!("postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres");

            let redis = testcontainers_modules::redis::Redis::default()
                .start()
                .await
                .expect("redis container");
            let redis_port = redis.get_host_port_ipv4(6379).await.unwrap();
            let redis_url = format!("redis://127.0.0.1:{redis_port}");

            // Run migrations once
            let pool = sqlx::PgPool::connect(&db_url).await.expect("pg connect");
            sqlx::migrate!("../../migrations")
                .run(&pool)
                .await
                .expect("migrations");
            pool.close().await;

            SharedContainers {
                _pg: pg,
                _redis: redis,
                db_url,
                redis_url,
            }
        })
        .await
}

/// A fully wired axum router backed by real Postgres + Redis containers,
/// plus a tonic gRPC server on a random port.
#[allow(dead_code)]
pub struct TestApp {
    pub router: axum::Router,
    pub grpc_addr: std::net::SocketAddr,
    _grpc_handle: tokio::task::JoinHandle<()>,
}

impl TestApp {
    pub async fn new() -> Self {
        let containers = shared_containers().await;

        let mut config = Config::new_for_test();
        config.database_url = containers.db_url.clone();
        config.read_database_url = containers.db_url.clone();
        config.redis_url = containers.redis_url.clone();

        let db = DbPools::from_config(&config).await.expect("db pools");

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

        let metrics_handle = PrometheusBuilder::new().build_recorder().handle();

        let router = build_router(state.clone(), metrics_handle);

        // Boot gRPC on a random available port
        let grpc_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("grpc listener");
        let grpc_addr = grpc_listener.local_addr().expect("grpc local addr");

        let grpc_server = build_grpc_server(state);
        let _grpc_handle = tokio::spawn(async move {
            grpc_server
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(
                    grpc_listener,
                ))
                .await
                .expect("grpc serve");
        });

        // Give the server a moment to bind
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        TestApp {
            router,
            grpc_addr,
            _grpc_handle,
        }
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
    async fn validate(&self, _content_type: &str, _content_id: Uuid) -> Result<bool, AppError> {
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
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}
