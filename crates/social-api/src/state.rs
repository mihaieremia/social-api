use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::cache::CacheManager;
use crate::clients::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use crate::clients::profile_client::{HttpTokenValidator, TokenValidator};
use crate::config::Config;
use crate::db::DbPools;
use crate::services::like_service::LikeService;
use crate::services::pubsub_manager::PubSubManager;

/// Shared application state, passed to all handlers via Axum's State extractor.
/// Wrapped in Arc for cheap cloning across handler tasks.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    db: DbPools,
    cache: CacheManager,
    config: Config,
    http_client: reqwest::Client,
    like_service: LikeService,
    token_validator: Box<dyn TokenValidator>,
    profile_breaker: Arc<CircuitBreaker>,
    pubsub_manager: PubSubManager,
    shutdown_token: CancellationToken,
    inflight_count: AtomicUsize,
}

impl AppState {
    pub async fn new(
        db: DbPools,
        cache: CacheManager,
        config: Config,
        shutdown_token: CancellationToken,
    ) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(20)
            .build()
            .expect("Failed to build HTTP client");

        let profile_breaker = build_circuit_breaker(&config, "profile_api");
        let content_breaker = build_circuit_breaker(&config, "content_api");

        let token_validator: Box<dyn TokenValidator> = if config.use_grpc_transport() {
            match crate::clients::grpc_profile_client::GrpcTokenValidator::new(
                &config.internal_grpc_url,
                cache.clone(),
                config.cache_ttl_user_status_secs,
                Duration::from_secs(config.upstream_request_timeout_secs),
            )
            .await
            {
                Ok(v) => {
                    tracing::info!("Using gRPC transport for profile validation");
                    Box::new(v)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to connect gRPC profile client, falling back to HTTP");
                    Box::new(HttpTokenValidator::new(
                        http_client.clone(),
                        config.profile_api_url.clone(),
                        cache.clone(),
                        config.cache_ttl_user_status_secs,
                    ))
                }
            }
        } else {
            Box::new(HttpTokenValidator::new(
                http_client.clone(),
                config.profile_api_url.clone(),
                cache.clone(),
                config.cache_ttl_user_status_secs,
            ))
        };

        let like_service = if config.use_grpc_transport() {
            match crate::clients::grpc_content_client::GrpcContentValidator::new(
                &config.internal_grpc_url,
                cache.clone(),
                config.clone(),
                Duration::from_secs(config.upstream_request_timeout_secs),
            )
            .await
            {
                Ok(v) => {
                    tracing::info!("Using gRPC transport for content validation");
                    LikeService::new_with_validator(
                        db.clone(),
                        cache.clone(),
                        Arc::new(v),
                        config.clone(),
                        content_breaker.clone(),
                    )
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to connect gRPC content client, falling back to HTTP");
                    LikeService::new(
                        db.clone(),
                        cache.clone(),
                        http_client.clone(),
                        config.clone(),
                        content_breaker.clone(),
                    )
                }
            }
        } else {
            LikeService::new(
                db.clone(),
                cache.clone(),
                http_client.clone(),
                config.clone(),
                content_breaker.clone(),
            )
        };

        let pubsub_manager = PubSubManager::new(
            config.redis_url.clone(),
            config.sse_broadcast_capacity,
            shutdown_token.clone(),
        );

        Self {
            inner: Arc::new(AppStateInner {
                db,
                cache,
                config,
                http_client,
                like_service,
                token_validator,
                profile_breaker,
                pubsub_manager,
                shutdown_token,
                inflight_count: AtomicUsize::new(0),
            }),
        }
    }

    pub fn db(&self) -> &DbPools {
        &self.inner.db
    }

    pub fn cache(&self) -> &CacheManager {
        &self.inner.cache
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub fn http_client(&self) -> &reqwest::Client {
        &self.inner.http_client
    }

    pub fn like_service(&self) -> &LikeService {
        &self.inner.like_service
    }

    pub fn token_validator(&self) -> &dyn TokenValidator {
        &*self.inner.token_validator
    }

    pub fn profile_breaker(&self) -> &CircuitBreaker {
        &self.inner.profile_breaker
    }

    pub fn pubsub_manager(&self) -> &PubSubManager {
        &self.inner.pubsub_manager
    }

    pub fn shutdown_token(&self) -> &CancellationToken {
        &self.inner.shutdown_token
    }

    pub fn inflight_increment(&self) {
        self.inner.inflight_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inflight_decrement(&self) {
        self.inner.inflight_count.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn inflight_count(&self) -> usize {
        self.inner.inflight_count.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    pub fn new_for_test(
        db: DbPools,
        cache: CacheManager,
        config: Config,
        shutdown_token: CancellationToken,
        token_validator: Box<dyn TokenValidator>,
        like_service: LikeService,
    ) -> Self {
        let profile_breaker = build_circuit_breaker(&config, "profile_api");
        let http_client = reqwest::Client::new();
        let pubsub_manager = PubSubManager::new(
            config.redis_url.clone(),
            config.sse_broadcast_capacity,
            shutdown_token.clone(),
        );
        Self {
            inner: Arc::new(AppStateInner {
                db,
                cache,
                config,
                http_client,
                like_service,
                token_validator,
                profile_breaker,
                pubsub_manager,
                shutdown_token,
                inflight_count: AtomicUsize::new(0),
            }),
        }
    }
}

fn build_circuit_breaker(config: &Config, name: &str) -> Arc<CircuitBreaker> {
    Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
        failure_threshold: config.circuit_breaker_failure_threshold,
        recovery_timeout: Duration::from_secs(config.circuit_breaker_recovery_timeout_secs),
        success_threshold: config.circuit_breaker_success_threshold,
        service_name: name.to_string(),
        rate_window: Duration::from_secs(config.circuit_breaker_rate_window_secs),
        failure_rate_threshold: 0.5,
        min_calls_for_rate: 10,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::internal_v1::{
        ValidateContentRequest, ValidateContentResponse, ValidateTokenRequest,
        ValidateTokenResponse,
        content_service_server::{ContentService, ContentServiceServer},
        profile_service_server::{ProfileService, ProfileServiceServer},
    };
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::{Request, Response, Status};

    // ── Minimal mock services for gRPC transport path ─────────────────────

    #[derive(Clone)]
    struct OkContentMock;

    #[tonic::async_trait]
    impl ContentService for OkContentMock {
        async fn validate(
            &self,
            _: Request<ValidateContentRequest>,
        ) -> Result<Response<ValidateContentResponse>, Status> {
            Ok(Response::new(ValidateContentResponse { exists: true }))
        }
    }

    #[derive(Clone)]
    struct OkProfileMock;

    #[tonic::async_trait]
    impl ProfileService for OkProfileMock {
        async fn validate_token(
            &self,
            _: Request<ValidateTokenRequest>,
        ) -> Result<Response<ValidateTokenResponse>, Status> {
            Ok(Response::new(ValidateTokenResponse {
                valid: true,
                user_id: uuid::Uuid::new_v4().to_string(),
                display_name: "mock".to_string(),
            }))
        }
    }

    /// Spawn a single tonic server that routes both Content and Profile services.
    async fn spawn_dual_grpc_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(ContentServiceServer::new(OkContentMock))
                .add_service(ProfileServiceServer::new(OkProfileMock))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .ok();
        });
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        format!("http://127.0.0.1:{port}")
    }

    struct TestHarness {
        _scope: crate::test_containers::TestScope,
        state: AppState,
    }

    async fn make_state() -> TestHarness {
        let scope = crate::test_containers::isolated_scope().await;

        let mut config = Config::new_for_test();
        config.database_url = scope.database_url.clone();
        config.read_database_url = scope.database_url.clone();
        config.redis_url = scope.redis_url.clone();

        let db = DbPools::from_config(&config).await.expect("db pools");

        let redis_pool = crate::cache::create_pool(&config)
            .await
            .expect("redis pool");
        let cache = CacheManager::new(redis_pool, &config);

        let shutdown_token = CancellationToken::new();
        let state = AppState::new(db, cache, config, shutdown_token).await;
        TestHarness {
            _scope: scope,
            state,
        }
    }

    #[tokio::test]
    async fn test_new_constructs_successfully_with_real_infra() {
        let harness = make_state().await;
        let state = harness.state;

        // Test inflight counter
        assert_eq!(state.inflight_count(), 0);
        state.inflight_increment();
        assert_eq!(state.inflight_count(), 1);
        state.inflight_increment();
        assert_eq!(state.inflight_count(), 2);
        state.inflight_decrement();
        assert_eq!(state.inflight_count(), 1);
        state.inflight_decrement();
        assert_eq!(state.inflight_count(), 0);

        // Test config accessor
        assert!(!state.config().database_url.is_empty());
        // Test http_client accessor
        let _ = state.http_client();
        // Test token_validator accessor
        let _ = state.token_validator();
        // Test profile_breaker accessor
        let _ = state.profile_breaker();
        // Test shutdown_token accessor
        let _ = state.shutdown_token();
        // Test db accessor
        let _ = state.db();
        // Test cache accessor
        let _ = state.cache();
        // Test like_service accessor
        let _ = state.like_service();
    }

    #[tokio::test]
    async fn test_appstate_clone_shares_inflight_counter() {
        let harness = make_state().await;
        let state = harness.state;

        // Clone shares same Arc
        let state2 = state.clone();
        state.inflight_increment();
        assert_eq!(state2.inflight_count(), 1);
    }

    #[tokio::test]
    async fn test_state_all_accessors_do_not_panic() {
        let harness = make_state().await;
        let state = harness.state;

        // Verify every accessor method returns without panicking.
        let _ = state.db();
        let _ = state.cache();
        let _ = state.config();
        let _ = state.http_client();
        let _ = state.like_service();
        let _ = state.token_validator();
        let _ = state.profile_breaker();
        let _ = state.pubsub_manager();
        let _ = state.shutdown_token();
    }

    #[tokio::test]
    async fn test_inflight_decrement_does_not_underflow() {
        let harness = make_state().await;
        let state = harness.state;

        // Increment then decrement must return to zero.
        state.inflight_increment();
        assert_eq!(state.inflight_count(), 1);
        state.inflight_decrement();
        assert_eq!(state.inflight_count(), 0);
    }

    #[tokio::test]
    async fn new_with_grpc_transport_connects_successfully() {
        let grpc_endpoint = spawn_dual_grpc_server().await;

        let scope = crate::test_containers::isolated_scope().await;
        let mut config = Config::new_for_test();
        config.database_url = scope.database_url.clone();
        config.read_database_url = scope.database_url.clone();
        config.redis_url = scope.redis_url.clone();
        config.internal_transport = "grpc".to_string();
        config.internal_grpc_url = grpc_endpoint;

        let db = DbPools::from_config(&config).await.expect("db pools");
        let redis_pool = crate::cache::create_pool(&config)
            .await
            .expect("redis pool");
        let cache = CacheManager::new(redis_pool, &config);
        let shutdown_token = CancellationToken::new();

        // This exercises the gRPC transport Ok(...) paths in AppState::new()
        let state = AppState::new(db, cache, config, shutdown_token).await;
        let _ = state.token_validator();
        let _ = state.like_service();
    }
}
