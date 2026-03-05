use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use std::collections::HashMap;

use shared::types::{HealthDetail, HealthResponse};

use crate::state::AppState;

/// GET /health/live
/// Returns 200 if process is running. No dependency checks.
/// Used by Kubernetes liveness probe.
#[utoipa::path(
    get,
    path = "/health/live",
    responses(
        (status = 200, description = "Service is alive", body = HealthResponse),
    ),
    tag = "Health"
)]
pub async fn liveness() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(HealthResponse {
            status: "ok".to_string(),
            details: None,
        }),
    )
}

/// GET /health/ready
/// Returns 200 only if ALL dependencies are reachable.
/// Returns 503 with details of what's failing.
/// Used by Kubernetes readiness probe.
#[utoipa::path(
    get,
    path = "/health/ready",
    responses(
        (status = 200, description = "All dependencies healthy", body = HealthResponse),
        (status = 503, description = "One or more dependencies unhealthy", body = HealthResponse),
    ),
    tag = "Health"
)]
pub async fn readiness(State(state): State<AppState>) -> impl IntoResponse {
    let mut details = HashMap::new();

    // Run all dependency checks in parallel so worst-case latency is
    // max(individual timeout) rather than sum of all timeouts.
    let (db_healthy, redis_healthy, content_healthy) = tokio::join!(
        state.db().is_healthy(),
        state.cache().is_healthy(),
        check_any_content_api(&state),
    );

    let all_healthy = db_healthy && redis_healthy && content_healthy;

    details.insert(
        "database".to_string(),
        HealthDetail {
            status: if db_healthy { "up" } else { "down" }.to_string(),
            error: if db_healthy {
                None
            } else {
                Some("Database unreachable".to_string())
            },
        },
    );

    details.insert(
        "redis".to_string(),
        HealthDetail {
            status: if redis_healthy { "up" } else { "down" }.to_string(),
            error: if redis_healthy {
                None
            } else {
                Some("Redis unreachable".to_string())
            },
        },
    );

    details.insert(
        "content_api".to_string(),
        HealthDetail {
            status: if content_healthy { "up" } else { "down" }.to_string(),
            error: if content_healthy {
                None
            } else {
                Some("No content API reachable".to_string())
            },
        },
    );

    let status_code = if all_healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status_code,
        Json(HealthResponse {
            status: if all_healthy {
                "ok".to_string()
            } else {
                "degraded".to_string()
            },
            details: Some(details),
        }),
    )
}

/// Check if at least one content API is reachable via its health endpoint.
async fn check_any_content_api(state: &AppState) -> bool {
    for url in state.config().content_api_urls.values() {
        let health_url = format!("{url}/health");
        match state
            .http_client()
            .get(&health_url)
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => return true,
            _ => continue,
        }
    }
    false
}
