use axum::{extract::Request, extract::State, middleware::Next, response::Response};

use crate::state::AppState;

/// Middleware that tracks in-flight requests for graceful shutdown drain.
pub async fn track_inflight(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    state.inflight_increment();
    let response = next.run(request).await;
    state.inflight_decrement();
    response
}
