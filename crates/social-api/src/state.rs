use std::sync::Arc;

use crate::cache::manager::CacheManager;
use crate::config::Config;
use crate::db::DbPools;

/// Shared application state, passed to all handlers via Axum's State extractor.
/// Wrapped in Arc for cheap cloning across handler tasks.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    pub db: DbPools,
    pub cache: CacheManager,
    pub config: Config,
    pub http_client: reqwest::Client,
}

impl AppState {
    pub fn new(db: DbPools, cache: CacheManager, config: Config) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .pool_max_idle_per_host(20)
            .build()
            .expect("Failed to build HTTP client");

        Self {
            inner: Arc::new(AppStateInner {
                db,
                cache,
                config,
                http_client,
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
}
