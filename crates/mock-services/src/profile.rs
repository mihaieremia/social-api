use axum::{
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde_json::json;

use crate::data;

/// GET /v1/auth/validate
/// Validates Bearer token and returns user identity.
pub async fn validate_token(req: Request) -> impl IntoResponse {
    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let token = auth_header.strip_prefix("Bearer ").unwrap_or("");

    for entry in data::tokens() {
        if entry.token == token {
            return (
                StatusCode::OK,
                Json(json!({
                    "valid": true,
                    "user_id": format!("usr_{}", entry.user_id),
                    "display_name": entry.display_name,
                })),
            )
                .into_response();
        }
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "valid": false,
            "error": "invalid_token",
        })),
    )
        .into_response()
}
