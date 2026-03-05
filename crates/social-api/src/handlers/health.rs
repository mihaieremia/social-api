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
pub async fn readiness(State(state): State<AppState>) -> impl IntoResponse {
    let mut details = HashMap::new();
    let mut all_healthy = true;

    // Check database
    let db_healthy = state.db().is_healthy().await;
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
    if !db_healthy {
        all_healthy = false;
    }

    // Check Redis
    let redis_healthy = state.cache().is_healthy().await;
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
    if !redis_healthy {
        all_healthy = false;
    }

    // Check at least one content API is reachable
    let content_healthy = check_any_content_api(&state).await;
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
    if !content_healthy {
        all_healthy = false;
    }

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
    for (_content_type, url) in &state.config().content_api_urls {
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
