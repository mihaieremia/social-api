use utoipa::OpenApi;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Social API",
        version = "1.0.0",
        description = "Social interactions API for BeInCrypto — likes, leaderboards, and real-time events across all content types.",
        license(name = "MIT"),
        contact(name = "BeInCrypto Engineering", email = "dani@beincrypto.com"),
    ),
    servers(
        (url = "http://localhost:8080", description = "Local development"),
    ),
    paths(
        crate::handlers::likes::like_content,
        crate::handlers::likes::unlike_content,
        crate::handlers::likes::get_count,
        crate::handlers::likes::get_status,
        crate::handlers::likes::get_user_likes,
        crate::handlers::likes::batch_counts,
        crate::handlers::likes::batch_statuses,
        crate::handlers::likes::get_leaderboard,
        crate::handlers::stream::like_stream,
        crate::handlers::health::liveness,
        crate::handlers::health::readiness,
        crate::handlers::metrics_handler::metrics,
    ),
    components(
        schemas(
            shared::types::LikeRequest,
            shared::types::LikeActionResponse,
            shared::types::LikeCountResponse,
            shared::types::LikeStatusResponse,
            shared::types::UserLikeItem,
            shared::types::PaginatedUserLikes,
            shared::types::BatchItem,
            shared::types::BatchRequest,
            shared::types::BatchCountResult,
            shared::types::BatchStatusResult,
            shared::types::BatchCountsResponse,
            shared::types::BatchStatusesResponse,
            shared::types::TopLikedItem,
            shared::types::TopLikedResponse,
            shared::types::HealthDetail,
            shared::types::HealthResponse,
            shared::types::LikeEvent,
            shared::types::TimeWindow,
            shared::errors::ApiError,
            shared::errors::ApiErrorBody,
            shared::errors::ErrorCode,
            crate::handlers::likes::LeaderboardParams,
        ),
    ),
    tags(
        (name = "Likes", description = "Like and unlike content items"),
        (name = "User Likes", description = "User's liked items with cursor pagination"),
        (name = "Batch", description = "Batch operations for content listings"),
        (name = "Leaderboard", description = "Top liked content by time window"),
        (name = "Stream", description = "Real-time Server-Sent Events for like updates"),
        (name = "Health", description = "Health checks and Prometheus metrics"),
    ),
    modifiers(&SecurityAddon),
)]
pub struct ApiDoc;

/// Adds Bearer auth security scheme to the OpenAPI spec.
struct SecurityAddon;

impl utoipa::Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer_auth",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("Token")
                        .description(Some(
                            "Session token from Profile API. Use tokens: tok_user_1 through tok_user_5 for testing."
                        ))
                        .build(),
                ),
            );
        }
    }
}
