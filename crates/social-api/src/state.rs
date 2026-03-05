use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::cache::manager::CacheManager;
use crate::clients::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use crate::clients::profile_client::HttpTokenValidator;
use crate::config::Config;
use crate::db::DbPools;
use crate::services::like_service::LikeService;

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
    token_validator: HttpTokenValidator,
    profile_breaker: Arc<CircuitBreaker>,
    content_breaker: Arc<CircuitBreaker>,
    shutdown_token: CancellationToken,
    inflight_count: AtomicUsize,
}

impl AppState {
    pub fn new(
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

        let profile_breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: config.circuit_breaker_failure_threshold,
            recovery_timeout: Duration::from_secs(config.circuit_breaker_recovery_timeout_secs),
            success_threshold: config.circuit_breaker_success_threshold,
            service_name: "profile_api".to_string(),
        }));

        let content_breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: config.circuit_breaker_failure_threshold,
            recovery_timeout: Duration::from_secs(config.circuit_breaker_recovery_timeout_secs),
            success_threshold: config.circuit_breaker_success_threshold,
            service_name: "content_api".to_string(),
        }));

        let token_validator =
            HttpTokenValidator::new(http_client.clone(), config.profile_api_url.clone());

        let like_service = LikeService::new(
            db.clone(),
            cache.clone(),
            http_client.clone(),
            config.clone(),
            content_breaker.clone(),
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
                content_breaker,
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

    pub fn token_validator(&self) -> &HttpTokenValidator {
        &self.inner.token_validator
    }

    pub fn profile_breaker(&self) -> &CircuitBreaker {
        &self.inner.profile_breaker
    }

    #[allow(dead_code)]
    pub fn content_breaker(&self) -> &CircuitBreaker {
        &self.inner.content_breaker
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
}
