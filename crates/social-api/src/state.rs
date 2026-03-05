use std::sync::Arc;

use crate::cache::manager::CacheManager;
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
}

impl AppState {
    pub fn new(db: DbPools, cache: CacheManager, config: Config) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .pool_max_idle_per_host(20)
            .build()
            .expect("Failed to build HTTP client");

        let like_service = LikeService::new(
            db.clone(),
            cache.clone(),
            http_client.clone(),
            config.clone(),
        );

        Self {
            inner: Arc::new(AppStateInner {
                db,
                cache,
                config,
                http_client,
                like_service,
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
}
