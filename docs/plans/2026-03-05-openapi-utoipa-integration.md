# OpenAPI + Swagger UI Integration via utoipa

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add full OpenAPI 3.1 schema generation and Swagger UI to the Social API using utoipa, with `ToSchema` on all request/response types and `#[utoipa::path]` on all handlers.

**Architecture:** utoipa generates the OpenAPI spec at compile time from Rust annotations. The spec is served at `/swagger-ui` via `utoipa-swagger-ui` (axum integration). All types in the shared crate derive `ToSchema`, and all handlers get `#[utoipa::path]` proc macros. A single `#[derive(OpenApi)]` struct in the social-api crate aggregates everything.

**Tech Stack:** `utoipa = "5"`, `utoipa-axum = "0.2"`, `utoipa-swagger-ui = "9"` (with `axum` feature), axum 0.8

---

### Task 1: Add utoipa dependencies to workspace

**Files:**
- Modify: `Cargo.toml` (workspace root, lines 11-49)
- Modify: `crates/shared/Cargo.toml`
- Modify: `crates/social-api/Cargo.toml`

**Step 1: Add utoipa to workspace dependencies**

In root `Cargo.toml`, add under `[workspace.dependencies]`:

```toml
# OpenAPI
utoipa = { version = "5", features = ["chrono", "uuid"] }
utoipa-axum = "0.2"
utoipa-swagger-ui = { version = "9", features = ["axum"] }
```

**Step 2: Add utoipa to shared crate**

In `crates/shared/Cargo.toml`, add:

```toml
utoipa = { workspace = true }
```

**Step 3: Add utoipa + utoipa-swagger-ui to social-api crate**

In `crates/social-api/Cargo.toml`, add:

```toml
utoipa = { workspace = true }
utoipa-axum = { workspace = true }
utoipa-swagger-ui = { workspace = true }
```

**Step 4: Verify it compiles**

Run: `cargo check -p shared -p social-api 2>&1 | tail -5`
Expected: no errors (warnings OK)

**Step 5: Commit**

```bash
git add Cargo.toml crates/shared/Cargo.toml crates/social-api/Cargo.toml
git commit -m "feat(openapi): add utoipa dependencies to workspace"
```

---

### Task 2: Derive ToSchema on shared types

**Files:**
- Modify: `crates/shared/src/types.rs`

**Step 1: Add utoipa import and derive ToSchema on all request/response structs**

Add `use utoipa::ToSchema;` at the top.

Derive `ToSchema` on every public struct/enum that appears in API request or response bodies. The following types need it:

```rust
// Add #[derive(ToSchema)] to:
// - LikeRequest
// - LikeActionResponse
// - LikeCountResponse
// - LikeStatusResponse
// - UserLikeItem
// - PaginatedResponse<T> (with #[aliases(PaginatedUserLikes = PaginatedResponse<UserLikeItem>)])
// - BatchItem
// - BatchRequest
// - BatchCountResult
// - BatchStatusResult
// - TopLikedItem
// - TopLikedResponse
// - HealthDetail
// - HealthResponse
// - PaginationParams
// - LikeEvent
// - TimeWindow
```

For `PaginatedResponse<T>`, use utoipa aliases since generics need concrete instantiation:

```rust
#[derive(Debug, Clone, Serialize, ToSchema)]
#[aliases(PaginatedUserLikes = PaginatedResponse<UserLikeItem>)]
pub struct PaginatedResponse<T: Serialize> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}
```

Add `#[schema(example = ...)]` annotations for key fields where useful:

```rust
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct LikeRequest {
    /// Content type (e.g. "post", "bonus_hunter", "top_picks")
    #[schema(example = "post")]
    pub content_type: String,
    /// UUID of the content item
    #[schema(example = "731b0395-4888-4822-b516-05b4b7bf2089")]
    pub content_id: Uuid,
}
```

**Step 2: Verify it compiles**

Run: `cargo check -p shared 2>&1 | tail -5`
Expected: no errors

**Step 3: Commit**

```bash
git add crates/shared/src/types.rs
git commit -m "feat(openapi): derive ToSchema on all shared types"
```

---

### Task 3: Derive ToSchema on error types

**Files:**
- Modify: `crates/shared/src/errors.rs`

**Step 1: Add ToSchema to error response types**

Add `use utoipa::ToSchema;` and derive `ToSchema` on the types that appear in error responses:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub enum ErrorCode {
    // ... variants stay the same
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApiError {
    pub error: ApiErrorBody,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApiErrorBody {
    /// Machine-readable error code
    pub code: ErrorCode,
    /// Human-readable error message
    pub message: String,
    /// Unique request identifier for tracing
    pub request_id: String,
    /// Additional context about the error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}
```

**Step 2: Verify it compiles**

Run: `cargo check -p shared 2>&1 | tail -5`
Expected: no errors

**Step 3: Commit**

```bash
git add crates/shared/src/errors.rs
git commit -m "feat(openapi): derive ToSchema on error types"
```

---

### Task 4: Add #[utoipa::path] to likes handlers

**Files:**
- Modify: `crates/social-api/src/handlers/likes.rs`

**Step 1: Annotate all handler functions**

Add utoipa path macros to each handler. The handler file needs `use shared::errors::ApiError;` for referencing the error schema. Also add `ToSchema` derive to `LeaderboardParams`.

```rust
use utoipa::ToSchema;

// LeaderboardParams needs ToSchema
#[derive(Debug, serde::Deserialize, ToSchema)]
pub struct LeaderboardParams {
    /// Filter by content type
    #[schema(example = "post")]
    pub content_type: Option<String>,
    /// Time window: 24h, 7d, 30d, all
    #[schema(example = "7d")]
    pub window: Option<String>,
    /// Max items to return (default 10, max 50)
    #[schema(example = 10)]
    pub limit: Option<i64>,
}

/// POST /v1/likes - Like content
#[utoipa::path(
    post,
    path = "/v1/likes",
    request_body = LikeRequest,
    responses(
        (status = 201, description = "Content liked successfully", body = LikeActionResponse),
        (status = 200, description = "Already liked (idempotent)", body = LikeActionResponse),
        (status = 400, description = "Invalid content type or ID", body = ApiError),
        (status = 401, description = "Unauthorized - missing or invalid token", body = ApiError),
        (status = 404, description = "Content not found", body = ApiError),
        (status = 429, description = "Rate limit exceeded", body = ApiError),
        (status = 503, description = "External dependency unavailable", body = ApiError),
    ),
    security(("bearer_auth" = [])),
    tag = "Likes"
)]
pub async fn like_content(...) -> ... { ... }

/// DELETE /v1/likes/{content_type}/{content_id} - Unlike content
#[utoipa::path(
    delete,
    path = "/v1/likes/{content_type}/{content_id}",
    params(
        ("content_type" = String, Path, description = "Content type", example = "post"),
        ("content_id" = Uuid, Path, description = "Content UUID", example = "731b0395-4888-4822-b516-05b4b7bf2089"),
    ),
    responses(
        (status = 200, description = "Content unliked successfully", body = LikeActionResponse),
        (status = 401, description = "Unauthorized", body = ApiError),
        (status = 404, description = "Content not found", body = ApiError),
        (status = 429, description = "Rate limit exceeded", body = ApiError),
    ),
    security(("bearer_auth" = [])),
    tag = "Likes"
)]
pub async fn unlike_content(...) -> ... { ... }

/// GET /v1/likes/{content_type}/{content_id}/count - Get like count
#[utoipa::path(
    get,
    path = "/v1/likes/{content_type}/{content_id}/count",
    params(
        ("content_type" = String, Path, description = "Content type", example = "post"),
        ("content_id" = Uuid, Path, description = "Content UUID", example = "731b0395-4888-4822-b516-05b4b7bf2089"),
    ),
    responses(
        (status = 200, description = "Like count retrieved", body = LikeCountResponse),
        (status = 400, description = "Invalid content type or ID", body = ApiError),
    ),
    tag = "Likes"
)]
pub async fn get_count(...) -> ... { ... }

/// GET /v1/likes/{content_type}/{content_id}/status - Get like status
#[utoipa::path(
    get,
    path = "/v1/likes/{content_type}/{content_id}/status",
    params(
        ("content_type" = String, Path, description = "Content type", example = "post"),
        ("content_id" = Uuid, Path, description = "Content UUID", example = "731b0395-4888-4822-b516-05b4b7bf2089"),
    ),
    responses(
        (status = 200, description = "Like status retrieved", body = LikeStatusResponse),
        (status = 401, description = "Unauthorized", body = ApiError),
        (status = 400, description = "Invalid content type or ID", body = ApiError),
    ),
    security(("bearer_auth" = [])),
    tag = "Likes"
)]
pub async fn get_status(...) -> ... { ... }

/// GET /v1/likes/user - Get user's liked items
#[utoipa::path(
    get,
    path = "/v1/likes/user",
    params(
        ("cursor" = Option<String>, Query, description = "Pagination cursor from previous response"),
        ("limit" = Option<i64>, Query, description = "Items per page (default 20, max 100)", example = 20),
        ("content_type" = Option<String>, Query, description = "Filter by content type", example = "post"),
    ),
    responses(
        (status = 200, description = "User's liked items", body = PaginatedUserLikes),
        (status = 401, description = "Unauthorized", body = ApiError),
        (status = 400, description = "Invalid cursor", body = ApiError),
    ),
    security(("bearer_auth" = [])),
    tag = "User Likes"
)]
pub async fn get_user_likes(...) -> ... { ... }

/// POST /v1/likes/batch/counts - Batch get like counts
#[utoipa::path(
    post,
    path = "/v1/likes/batch/counts",
    request_body = BatchRequest,
    responses(
        (status = 200, description = "Batch like counts", body = BatchCountsResponse),
        (status = 400, description = "Batch too large (max 100)", body = ApiError),
    ),
    tag = "Batch"
)]
pub async fn batch_counts(...) -> ... { ... }

/// POST /v1/likes/batch/statuses - Batch get like statuses
#[utoipa::path(
    post,
    path = "/v1/likes/batch/statuses",
    request_body = BatchRequest,
    responses(
        (status = 200, description = "Batch like statuses", body = BatchStatusesResponse),
        (status = 400, description = "Batch too large (max 100)", body = ApiError),
        (status = 401, description = "Unauthorized", body = ApiError),
    ),
    security(("bearer_auth" = [])),
    tag = "Batch"
)]
pub async fn batch_statuses(...) -> ... { ... }

/// GET /v1/likes/top - Top liked content leaderboard
#[utoipa::path(
    get,
    path = "/v1/likes/top",
    params(LeaderboardParams),
    responses(
        (status = 200, description = "Top liked content", body = TopLikedResponse),
        (status = 400, description = "Invalid window parameter", body = ApiError),
    ),
    tag = "Leaderboard"
)]
pub async fn get_leaderboard(...) -> ... { ... }
```

**Note:** For batch responses that use `Json(serde_json::json!({ "results": ... }))`, we need wrapper response types. Add these to `crates/shared/src/types.rs`:

```rust
/// Batch counts response wrapper.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BatchCountsResponse {
    pub results: Vec<BatchCountResult>,
}

/// Batch statuses response wrapper.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BatchStatusesResponse {
    pub results: Vec<BatchStatusResult>,
}
```

Then update the handlers to use `Json(BatchCountsResponse { results })` and `Json(BatchStatusesResponse { results })` instead of `serde_json::json!`.

**Step 2: Verify it compiles**

Run: `cargo check -p social-api 2>&1 | tail -10`
Expected: no errors

**Step 3: Commit**

```bash
git add crates/shared/src/types.rs crates/social-api/src/handlers/likes.rs
git commit -m "feat(openapi): annotate likes handlers with utoipa::path"
```

---

### Task 5: Add #[utoipa::path] to stream and health handlers

**Files:**
- Modify: `crates/social-api/src/handlers/stream.rs`
- Modify: `crates/social-api/src/handlers/health.rs`
- Modify: `crates/social-api/src/handlers/metrics_handler.rs`

**Step 1: Annotate stream handler**

Add `ToSchema` to `StreamParams` and annotate the handler:

```rust
use utoipa::ToSchema;

#[derive(Debug, Deserialize, ToSchema)]
pub struct StreamParams {
    /// Content type to subscribe to
    #[schema(example = "post")]
    pub content_type: String,
    /// Content ID to subscribe to
    #[schema(example = "731b0395-4888-4822-b516-05b4b7bf2089")]
    pub content_id: String,
}

/// GET /v1/likes/stream - SSE event stream for real-time like updates
#[utoipa::path(
    get,
    path = "/v1/likes/stream",
    params(
        ("content_type" = String, Query, description = "Content type", example = "post"),
        ("content_id" = String, Query, description = "Content UUID", example = "731b0395-4888-4822-b516-05b4b7bf2089"),
    ),
    responses(
        (status = 200, description = "SSE stream opened. Events: like, unlike, heartbeat, shutdown",
         content_type = "text/event-stream"),
    ),
    tag = "Stream"
)]
pub async fn like_stream(...) -> ... { ... }
```

**Step 2: Annotate health handlers**

```rust
/// GET /health/live - Liveness probe
#[utoipa::path(
    get,
    path = "/health/live",
    responses(
        (status = 200, description = "Service is alive", body = HealthResponse),
    ),
    tag = "Health"
)]
pub async fn liveness() -> ... { ... }

/// GET /health/ready - Readiness probe
#[utoipa::path(
    get,
    path = "/health/ready",
    responses(
        (status = 200, description = "All dependencies healthy", body = HealthResponse),
        (status = 503, description = "One or more dependencies unhealthy", body = HealthResponse),
    ),
    tag = "Health"
)]
pub async fn readiness(...) -> ... { ... }
```

**Step 3: Annotate metrics handler**

```rust
/// GET /metrics - Prometheus metrics
#[utoipa::path(
    get,
    path = "/metrics",
    responses(
        (status = 200, description = "Prometheus exposition format metrics",
         content_type = "text/plain; version=0.0.4"),
    ),
    tag = "Health"
)]
pub async fn metrics(...) -> ... { ... }
```

**Step 4: Verify it compiles**

Run: `cargo check -p social-api 2>&1 | tail -10`
Expected: no errors

**Step 5: Commit**

```bash
git add crates/social-api/src/handlers/stream.rs crates/social-api/src/handlers/health.rs crates/social-api/src/handlers/metrics_handler.rs
git commit -m "feat(openapi): annotate stream, health, and metrics handlers"
```

---

### Task 6: Create OpenApi struct and mount Swagger UI

**Files:**
- Create: `crates/social-api/src/openapi.rs`
- Modify: `crates/social-api/src/main.rs`
- Modify: `crates/social-api/src/server.rs`

**Step 1: Create the OpenApi definition module**

Create `crates/social-api/src/openapi.rs`:

```rust
use utoipa::OpenApi;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};

use shared::errors::{ApiError, ApiErrorBody, ErrorCode};
use shared::types::*;

use crate::handlers;

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
        handlers::likes::like_content,
        handlers::likes::unlike_content,
        handlers::likes::get_count,
        handlers::likes::get_status,
        handlers::likes::get_user_likes,
        handlers::likes::batch_counts,
        handlers::likes::batch_statuses,
        handlers::likes::get_leaderboard,
        handlers::stream::like_stream,
        handlers::health::liveness,
        handlers::health::readiness,
        handlers::metrics_handler::metrics,
    ),
    components(
        schemas(
            LikeRequest,
            LikeActionResponse,
            LikeCountResponse,
            LikeStatusResponse,
            UserLikeItem,
            PaginatedUserLikes,
            BatchItem,
            BatchRequest,
            BatchCountResult,
            BatchStatusResult,
            BatchCountsResponse,
            BatchStatusesResponse,
            TopLikedItem,
            TopLikedResponse,
            HealthDetail,
            HealthResponse,
            LikeEvent,
            TimeWindow,
            ApiError,
            ApiErrorBody,
            ErrorCode,
            handlers::likes::LeaderboardParams,
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
```

**Step 2: Mount Swagger UI in server.rs**

Modify `server.rs` to add the Swagger UI route:

```rust
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::openapi::ApiDoc;

// In build_router(), after the final Router::new():
Router::new()
    .merge(health_routes)
    .merge(metrics_route)
    .merge(api_routes)
    .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
```

**Step 3: Register the module in main.rs**

Add `mod openapi;` to the module declarations in `main.rs`.

**Step 4: Verify it compiles**

Run: `cargo check -p social-api 2>&1 | tail -10`
Expected: no errors

**Step 5: Commit**

```bash
git add crates/social-api/src/openapi.rs crates/social-api/src/server.rs crates/social-api/src/main.rs
git commit -m "feat(openapi): create OpenApi struct and mount Swagger UI at /swagger-ui"
```

---

### Task 7: Build and verify end-to-end

**Step 1: Full build**

Run: `cargo build -p social-api 2>&1 | tail -10`
Expected: compiles successfully

**Step 2: Run existing tests**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: all existing tests pass

**Step 3: Docker compose test (if containers available)**

Run: `docker compose up --build -d && sleep 5 && curl -s http://localhost:8080/api-docs/openapi.json | head -20`
Expected: JSON OpenAPI spec returned

Run: `curl -s -o /dev/null -w "%{http_code}" http://localhost:8080/swagger-ui/`
Expected: 200

**Step 4: Commit (if any fixups needed)**

```bash
git add -A
git commit -m "fix(openapi): resolve compilation issues from integration"
```

---

## File Change Summary

| File | Action | Description |
|---|---|---|
| `Cargo.toml` | Modify | Add utoipa workspace deps |
| `crates/shared/Cargo.toml` | Modify | Add utoipa dep |
| `crates/social-api/Cargo.toml` | Modify | Add utoipa + swagger-ui deps |
| `crates/shared/src/types.rs` | Modify | Derive ToSchema on all types |
| `crates/shared/src/errors.rs` | Modify | Derive ToSchema on error types |
| `crates/social-api/src/handlers/likes.rs` | Modify | Add #[utoipa::path] to all handlers |
| `crates/social-api/src/handlers/stream.rs` | Modify | Add #[utoipa::path] to SSE handler |
| `crates/social-api/src/handlers/health.rs` | Modify | Add #[utoipa::path] to health handlers |
| `crates/social-api/src/handlers/metrics_handler.rs` | Modify | Add #[utoipa::path] to metrics handler |
| `crates/social-api/src/openapi.rs` | Create | OpenApi derive struct + SecurityAddon |
| `crates/social-api/src/server.rs` | Modify | Mount SwaggerUi |
| `crates/social-api/src/main.rs` | Modify | Add `mod openapi` |
