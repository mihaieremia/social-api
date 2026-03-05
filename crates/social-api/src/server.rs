use axum::{Router, routing::get};

use crate::handlers;
use crate::state::AppState;

/// Build the Axum router with all routes and middleware.
pub fn build_router(state: AppState) -> Router {
    // Health check routes (no middleware)
    let health_routes = Router::new()
        .route("/health/live", get(handlers::health::liveness))
        .route("/health/ready", get(handlers::health::readiness));

    // API routes (will add more in subsequent tasks)
    let api_routes = Router::new();

    Router::new()
        .merge(health_routes)
        .merge(api_routes)
        .with_state(state)
}
