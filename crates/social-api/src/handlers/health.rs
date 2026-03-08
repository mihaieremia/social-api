use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use std::collections::HashMap;

use shared::types::{HealthDetail, HealthResponse};

use crate::health;
use crate::state::AppState;

/// Build a HealthDetail for a single dependency check.
fn make_health_detail(healthy: bool, down_message: &str) -> HealthDetail {
    HealthDetail {
        status: if healthy { "up" } else { "down" }.to_string(),
        error: if healthy {
            None
        } else {
            Some(down_message.to_string())
        },
    }
}

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
    let health = health::check_dependencies(&state).await;
    let all_healthy = health.all_healthy();

    details.insert(
        "database".to_string(),
        make_health_detail(health.database, "Database unreachable"),
    );
    details.insert(
        "redis".to_string(),
        make_health_detail(health.redis, "Redis unreachable"),
    );
    details.insert(
        "content_api".to_string(),
        make_health_detail(health.content_api, "No content API reachable"),
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
