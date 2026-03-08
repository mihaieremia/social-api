use crate::state::AppState;

pub struct DependencyHealth {
    pub database: bool,
    pub redis: bool,
    pub content_api: bool,
}

impl DependencyHealth {
    pub fn all_healthy(&self) -> bool {
        self.database && self.redis && self.content_api
    }
}

pub async fn check_dependencies(state: &AppState) -> DependencyHealth {
    let (database, redis, content_api) = tokio::join!(
        state.db().is_healthy(),
        state.cache().is_healthy(),
        check_any_content_api(state),
    );

    DependencyHealth {
        database,
        redis,
        content_api,
    }
}

async fn check_any_content_api(state: &AppState) -> bool {
    for url in state.config().content_api_urls.values() {
        let health_url = format!("{url}/health");
        let response = state
            .http_client()
            .get(&health_url)
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await;

        if matches!(response, Ok(resp) if resp.status().is_success()) {
            return true;
        }
    }

    false
}
