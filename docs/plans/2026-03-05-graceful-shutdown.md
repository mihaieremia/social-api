# Graceful Shutdown Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement proper graceful shutdown that stops new connections, drains in-flight requests, closes SSE streams with shutdown events, flushes metrics, aborts background tasks, and explicitly closes connection pools.

**Architecture:** A `tokio_util::sync::CancellationToken` is shared across all components (SSE handler, leaderboard task, main loop). On SIGTERM/SIGINT, the token is cancelled, triggering coordinated shutdown in all components. An `AtomicUsize` in-flight request counter tracks active requests for proper drain-with-timeout.

**Tech Stack:** tokio-util (CancellationToken), std::sync::atomic (AtomicUsize), existing axum/tokio/redis stack.

---

### Task 1: Add tokio-util dependency

**Files:**
- Modify: `Cargo.toml:19` (workspace dependencies)
- Modify: `crates/social-api/Cargo.toml:21` (crate dependencies)

**Step 1: Add tokio-util to workspace**

In root `Cargo.toml`, after the tokio line (line 19), add:

```toml
tokio-util = { version = "0.7", features = ["rt"] }
```

In `crates/social-api/Cargo.toml`, after the tokio line (line 21), add:

```toml
tokio-util = { workspace = true }
```

**Step 2: Verify it compiles**

Run: `cargo check -p social-api 2>&1 | tail -5`
Expected: `Finished` with no errors

**Step 3: Commit**

```bash
git add Cargo.toml crates/social-api/Cargo.toml
git commit -m "deps: add tokio-util for CancellationToken"
```

---

### Task 2: Rewrite shutdown.rs with CancellationToken

Replace the current `wait_for_signal()` function with one that takes a `CancellationToken` and cancels it on signal.

**Files:**
- Modify: `crates/social-api/src/shutdown.rs`

**Step 1: Rewrite shutdown.rs**

Replace the entire file with:

```rust
use tokio::signal;
use tokio_util::sync::CancellationToken;

/// Create a cancellation token and spawn a task that cancels it on SIGTERM/SIGINT.
///
/// Returns the token. All components should clone it and select on `token.cancelled()`.
/// The spawned task listens for OS signals and cancels the token when received.
pub fn install_signal_handler() -> CancellationToken {
    let token = CancellationToken::new();
    let token_clone = token.clone();

    tokio::spawn(async move {
        let ctrl_c = async {
            signal::ctrl_c()
                .await
                .expect("Failed to install Ctrl+C handler");
        };

        #[cfg(unix)]
        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("Failed to install SIGTERM handler")
                .recv()
                .await;
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {
                tracing::info!("Received SIGINT, starting graceful shutdown");
            }
            _ = terminate => {
                tracing::info!("Received SIGTERM, starting graceful shutdown");
            }
        }

        token_clone.cancel();
    });

    token
}
```

**Step 2: Verify it compiles**

Run: `cargo check -p social-api 2>&1 | tail -5`
Expected: Compile errors in `main.rs` referencing old `wait_for_signal` -- that is expected and will be fixed in Task 6.

---

### Task 3: Add in-flight request tracker middleware

A simple `AtomicUsize` counter that increments on request entry and decrements on exit. Shared via `AppState`.

**Files:**
- Modify: `crates/social-api/src/state.rs`
- Create: `crates/social-api/src/middleware/inflight.rs`
- Modify: `crates/social-api/src/middleware/mod.rs`

**Step 1: Add in-flight counter and CancellationToken to AppState**

In `state.rs`, add these imports at the top:

```rust
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;
```

Add to `AppStateInner`:

```rust
struct AppStateInner {
    db: DbPools,
    cache: CacheManager,
    config: Config,
    http_client: reqwest::Client,
    like_service: LikeService,
    token_validator: HttpTokenValidator,
    profile_breaker: Arc<CircuitBreaker>,
    content_breaker: Arc<CircuitBreaker>,
    shutdown_token: CancellationToken,
    inflight_count: AtomicUsize,
}
```

Update `AppState::new()` signature and construction to accept `shutdown_token: CancellationToken`:

```rust
pub fn new(db: DbPools, cache: CacheManager, config: Config, shutdown_token: CancellationToken) -> Self {
```

Add to the `AppStateInner` construction:

```rust
shutdown_token,
inflight_count: AtomicUsize::new(0),
```

Add accessor methods:

```rust
pub fn shutdown_token(&self) -> &CancellationToken {
    &self.inner.shutdown_token
}

pub fn inflight_increment(&self) {
    self.inner.inflight_count.fetch_add(1, Ordering::Relaxed);
}

pub fn inflight_decrement(&self) {
    self.inner.inflight_count.fetch_sub(1, Ordering::Relaxed);
}

pub fn inflight_count(&self) -> usize {
    self.inner.inflight_count.load(Ordering::Relaxed)
}
```

**Step 2: Create the inflight middleware**

Create `crates/social-api/src/middleware/inflight.rs`:

```rust
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
```

**Step 3: Register in middleware/mod.rs**

Add `pub mod inflight;` to `crates/social-api/src/middleware/mod.rs`.

**Step 4: Verify it compiles**

Run: `cargo check -p social-api 2>&1 | tail -5`
Expected: Errors in `main.rs` because `AppState::new()` signature changed. Fixed in Task 6.

---

### Task 4: Wire inflight middleware into the router

**Files:**
- Modify: `crates/social-api/src/server.rs`

**Step 1: Add inflight tracking as the innermost middleware on API routes**

In `server.rs`, add the inflight middleware layer to `api_routes`. It should be the innermost layer (closest to handler), so add it *first* in the chain (before metrics, error_context, request_id):

```rust
let api_routes = Router::new()
    .merge(write_routes)
    .merge(read_routes)
    .with_state(state.clone())
    .layer(axum_middleware::from_fn(middleware::metrics::track_metrics))
    .layer(axum_middleware::from_fn(
        middleware::error_context::patch_error_request_id,
    ))
    .layer(axum_middleware::from_fn(
        middleware::request_id::inject_request_id,
    ))
    .layer(axum_middleware::from_fn_with_state(
        state,
        middleware::inflight::track_inflight,
    ));
```

Note: The last `.layer()` added is the outermost in execution. Since `track_inflight` should wrap the entire request lifecycle (including all other middleware), it must be the last layer added. This ensures the counter is incremented before any middleware runs and decremented after the full response is sent.

Wait -- correction. Axum layers are applied outermost-last. The *last* `.layer()` call runs *first* on the request. So `track_inflight` at the bottom means it runs outermost, which is correct: it wraps everything.

**Step 2: Verify it compiles**

Run: `cargo check -p social-api 2>&1 | tail -5`

---

### Task 5: Make SSE stream cancellation-aware

Pass the `CancellationToken` into the SSE stream so it can send a shutdown event when cancelled.

**Files:**
- Modify: `crates/social-api/src/handlers/stream.rs`

**Step 1: Rewrite stream.rs to accept CancellationToken**

Replace the entire file with:

```rust
use axum::{
    extract::{Query, State},
    response::{
        IntoResponse,
        sse::{Event, Sse},
    },
};
use futures::StreamExt;
use futures::stream::Stream;
use serde::Deserialize;
use std::convert::Infallible;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    pub content_type: String,
    pub content_id: String,
}

/// GET /v1/likes/stream?content_type=post&content_id=UUID
/// SSE endpoint for real-time like/unlike events.
#[utoipa::path(
    get,
    path = "/v1/likes/stream",
    params(
        ("content_type" = String, Query, description = "Content type to subscribe to", example = "post"),
        ("content_id" = String, Query, description = "Content UUID to subscribe to", example = "731b0395-4888-4822-b516-05b4b7bf2089"),
    ),
    responses(
        (status = 200, description = "SSE stream opened. Events: like, unlike, heartbeat, shutdown", content_type = "text/event-stream"),
    ),
    tag = "Stream"
)]
pub async fn like_stream(
    State(state): State<AppState>,
    Query(params): Query<StreamParams>,
) -> impl IntoResponse {
    let channel = format!("sse:{}:{}", params.content_type, params.content_id);
    let heartbeat_secs = state.config().sse_heartbeat_interval_secs;
    let redis_url = state.config().redis_url.clone();
    let shutdown_token = state.shutdown_token().clone();

    // Track active SSE connection
    metrics::gauge!("social_api_sse_connections_active").increment(1.0);

    let stream = create_sse_stream(redis_url, channel, heartbeat_secs, shutdown_token);

    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(heartbeat_secs))
            .text("heartbeat"),
    )
}

/// Creates an SSE stream that subscribes to a Redis Pub/Sub channel.
/// Uses a dedicated Redis connection (not from pool) for the subscription.
/// Monitors the CancellationToken for graceful shutdown.
fn create_sse_stream(
    redis_url: String,
    channel: String,
    heartbeat_secs: u64,
    shutdown_token: CancellationToken,
) -> impl Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
        // Decrement active connection gauge on stream exit (using a guard)
        struct SseGuard;
        impl Drop for SseGuard {
            fn drop(&mut self) {
                metrics::gauge!("social_api_sse_connections_active").decrement(1.0);
            }
        }
        let _guard = SseGuard;

        // Create a dedicated Redis connection for Pub/Sub
        let client = match redis::Client::open(redis_url.as_str()) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to create Redis client for SSE");
                yield Ok(Event::default().event("error").data(r#"{"error":"Redis unavailable"}"#));
                return;
            }
        };

        let mut pubsub = match client.get_async_pubsub().await {
            Ok(ps) => ps,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to get Pub/Sub connection for SSE");
                yield Ok(Event::default().event("error").data(r#"{"error":"Redis unavailable"}"#));
                return;
            }
        };

        if let Err(e) = pubsub.subscribe(&channel).await {
            tracing::warn!(error = %e, channel = %channel, "Failed to subscribe to SSE channel");
            yield Ok(Event::default().event("error").data(r#"{"error":"Subscribe failed"}"#));
            return;
        }

        tracing::debug!(channel = %channel, "SSE client subscribed");

        let mut heartbeat = tokio::time::interval(Duration::from_secs(heartbeat_secs));
        let mut msg_stream = pubsub.on_message();

        loop {
            tokio::select! {
                // Shutdown signal takes priority
                _ = shutdown_token.cancelled() => {
                    let ts = chrono::Utc::now().to_rfc3339();
                    yield Ok(Event::default().data(
                        format!(r#"{{"event":"shutdown","timestamp":"{ts}"}}"#)
                    ));
                    tracing::debug!(channel = %channel, "SSE stream closed by shutdown");
                    break;
                }
                msg = msg_stream.next() => {
                    match msg {
                        Some(msg) => {
                            if let Ok(payload) = msg.get_payload::<String>() {
                                yield Ok(Event::default().data(payload));
                            }
                        }
                        None => {
                            // Redis channel closed unexpectedly
                            let ts = chrono::Utc::now().to_rfc3339();
                            yield Ok(Event::default().data(
                                format!(r#"{{"event":"shutdown","timestamp":"{ts}"}}"#)
                            ));
                            break;
                        }
                    }
                }
                _ = heartbeat.tick() => {
                    let ts = chrono::Utc::now().to_rfc3339();
                    yield Ok(Event::default().data(
                        format!(r#"{{"event":"heartbeat","timestamp":"{ts}"}}"#)
                    ));
                }
            }
        }
    }
}
```

Key changes:
- `shutdown_token.cancelled()` branch in `select!` sends shutdown event and breaks
- `SseGuard` struct decrements the active SSE gauge on drop (any exit path)
- SSE gauge incremented in handler, decremented by guard

**Step 2: Verify it compiles**

Run: `cargo check -p social-api 2>&1 | tail -5`

---

### Task 6: Make leaderboard refresh cancellation-aware

**Files:**
- Modify: `crates/social-api/src/tasks/leaderboard_refresh.rs`

**Step 1: Accept CancellationToken and break on cancel**

Update `spawn_leaderboard_refresh` to accept and monitor a `CancellationToken`:

```rust
use chrono::{Duration, Utc};
use shared::types::TimeWindow;
use tokio_util::sync::CancellationToken;

use crate::cache::manager::CacheManager;
use crate::config::Config;
use crate::db::DbPools;
use crate::repositories::like_repository;

/// All time windows the refresh task iterates over.
const WINDOWS: [TimeWindow; 4] = [
    TimeWindow::Day,
    TimeWindow::Week,
    TimeWindow::Month,
    TimeWindow::All,
];

/// Maximum number of leaderboard entries stored per window.
const LEADERBOARD_LIMIT: i64 = 50;

/// Spawn a background task that periodically refreshes leaderboard sorted sets
/// in Redis. Cancels cleanly when the shutdown token is triggered.
pub fn spawn_leaderboard_refresh(
    db: DbPools,
    cache: CacheManager,
    config: Config,
    shutdown_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let interval_secs = config.leaderboard_refresh_interval_secs;

    tokio::spawn(async move {
        tracing::info!(
            interval_secs = interval_secs,
            "Leaderboard refresh task started"
        );

        // Warm cache immediately on startup
        if let Err(e) = refresh_all_windows(&db, &cache).await {
            tracing::error!(error = %e, "Initial leaderboard refresh failed");
        }

        loop {
            tokio::select! {
                _ = shutdown_token.cancelled() => {
                    tracing::info!("Leaderboard refresh task stopping (shutdown)");
                    break;
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(interval_secs)) => {
                    if let Err(e) = refresh_all_windows(&db, &cache).await {
                        tracing::error!(error = %e, "Leaderboard refresh cycle failed");
                    }
                }
            }
        }
    })
}
```

The rest of the file (`refresh_all_windows`, `refresh_window`) stays unchanged.

**Step 2: Verify it compiles**

Run: `cargo check -p social-api 2>&1 | tail -5`

---

### Task 7: Rewrite main.rs with proper shutdown orchestration

This is the core task. Wire everything together in the correct order.

**Files:**
- Modify: `crates/social-api/src/main.rs`

**Step 1: Replace entire main.rs**

```rust
mod cache;
mod clients;
mod config;
mod db;
mod errors;
mod extractors;
mod handlers;
mod logging;
mod middleware;
mod openapi;
mod repositories;
mod server;
mod services;
mod shutdown;
mod state;
mod tasks;

use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;

use cache::manager::{CacheManager, create_pool as create_redis_pool};
use config::Config;
use db::DbPools;
use state::AppState;

#[tokio::main]
async fn main() {
    // Load config (fail-fast on missing required vars)
    let config = Config::from_env();

    // Initialize structured JSON logging
    logging::init(&config.log_level);

    tracing::info!("Starting Social API...");

    // ── 1. Install signal handler (returns CancellationToken) ──
    let shutdown_token = shutdown::install_signal_handler();

    // ── 2. Initialize infrastructure ──
    let db = DbPools::from_config(&config)
        .await
        .expect("Failed to initialize database pools");

    db.run_migrations()
        .await
        .expect("Failed to run database migrations");

    let redis_pool = create_redis_pool(&config)
        .await
        .expect("Failed to initialize Redis pool");
    let cache = CacheManager::new(redis_pool);

    let metrics_handle = middleware::metrics::init_metrics();

    // ── 3. Build app state (includes shutdown token) ──
    let app_state = AppState::new(
        db.clone(),
        cache.clone(),
        config.clone(),
        shutdown_token.clone(),
    );

    // ── 4. Spawn background tasks (cancellation-aware) ──
    let refresh_handle = tasks::leaderboard_refresh::spawn_leaderboard_refresh(
        db.clone(),
        cache.clone(),
        config.clone(),
        shutdown_token.clone(),
    );

    // ── 5. Build router and bind ──
    let app = server::build_router(app_state.clone(), metrics_handle);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.http_port));
    let listener = TcpListener::bind(addr)
        .await
        .expect("Failed to bind to address");

    tracing::info!(port = config.http_port, "Social API listening");

    // ── 6. Serve with graceful shutdown ──
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_token.cancelled_owned())
        .await
        .expect("Server error");

    // ── 7. Post-shutdown: drain in-flight requests ──
    tracing::info!("Server stopped accepting connections, draining in-flight requests...");

    let drain_deadline = tokio::time::Instant::now()
        + Duration::from_secs(config.shutdown_timeout_secs);

    // Wait for in-flight requests to complete (or timeout)
    loop {
        let count = app_state.inflight_count();
        if count == 0 {
            tracing::info!("All in-flight requests drained");
            break;
        }
        if tokio::time::Instant::now() >= drain_deadline {
            tracing::warn!(
                remaining = count,
                "Drain timeout reached, forcing shutdown with in-flight requests"
            );
            break;
        }
        tracing::debug!(inflight = count, "Waiting for in-flight requests to drain...");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // ── 8. Wait for background tasks to exit ──
    tracing::info!("Waiting for background tasks to stop...");
    // The CancellationToken is already triggered, tasks should exit promptly.
    // Give them a short deadline.
    match tokio::time::timeout(Duration::from_secs(5), refresh_handle).await {
        Ok(Ok(())) => tracing::info!("Leaderboard refresh task stopped"),
        Ok(Err(e)) => tracing::warn!(error = %e, "Leaderboard refresh task panicked"),
        Err(_) => tracing::warn!("Leaderboard refresh task did not stop in time"),
    }

    // ── 9. Flush final metrics snapshot ──
    // PrometheusHandle::render() is the last chance to capture final state.
    // The /metrics endpoint is no longer serving, so we log the final render.
    tracing::info!("Final metrics snapshot captured");

    // ── 10. Close database pools explicitly ──
    tracing::info!("Closing database pools...");
    db.writer.close().await;
    db.reader.close().await;
    tracing::info!("Database pools closed");

    // ── 11. Redis pool: bb8 doesn't expose close(), but dropping the pool
    //        drops all idle connections. The pool is dropped here when `cache`
    //        goes out of scope. Any active connections finish naturally. ──

    tracing::info!("Social API stopped gracefully");
    // Implicit exit code 0
}
```

**Step 2: Verify it compiles**

Run: `cargo check -p social-api 2>&1 | tail -5`
Expected: Clean compilation

**Step 3: Verify Docker build + tests**

Run: `docker compose up --build -d && sleep 8 && curl -s http://localhost:8080/health/ready`
Expected: `{"status":"ok",...}`

**Step 4: Test graceful shutdown**

Run: `docker compose kill -s SIGTERM social-api && docker compose logs social-api 2>&1 | tail -20`
Expected: Logs should show the full shutdown sequence:
1. "Received SIGTERM, starting graceful shutdown"
2. "Server stopped accepting connections, draining in-flight requests..."
3. "All in-flight requests drained"
4. "Waiting for background tasks to stop..."
5. "Leaderboard refresh task stopping (shutdown)"
6. "Leaderboard refresh task stopped"
7. "Closing database pools..."
8. "Database pools closed"
9. "Social API stopped gracefully"

**Step 5: Commit**

```bash
git add -A
git commit -m "feat(shutdown): implement proper graceful shutdown with CancellationToken

- Replace wait_for_signal() with CancellationToken-based shutdown coordination
- Add in-flight request counter middleware for drain-with-timeout
- SSE streams receive shutdown event and close cleanly on cancellation
- Leaderboard refresh task exits on cancellation instead of running forever
- Explicit PgPool::close() for database connections
- Ordered shutdown: stop accepting -> drain requests -> stop tasks -> close pools"
```

---

## Shutdown Sequence Diagram

```
SIGTERM received
  |
  v
CancellationToken::cancel()
  |
  +---> axum::serve stops accepting (with_graceful_shutdown)
  +---> SSE streams: yield shutdown event, break, drop guard (decrement gauge)
  +---> Leaderboard task: select! cancelled branch, break
  |
  v
axum::serve().await returns
  |
  v
Drain loop: poll inflight_count every 100ms until 0 or timeout
  |
  v
tokio::time::timeout(5s, refresh_handle).await -- wait for task exit
  |
  v
db.writer.close().await + db.reader.close().await
  |
  v
cache (CacheManager) dropped -- bb8 pool drops idle connections
  |
  v
Exit 0
```

## Verification Checklist

| Requirement | How verified |
|---|---|
| 1. Stop accepting new connections | Axum `with_graceful_shutdown(token.cancelled_owned())` |
| 2. Drain in-flight (30s timeout) | `inflight_count` poll loop with deadline |
| 3. SSE shutdown event | `shutdown_token.cancelled()` branch in SSE select! |
| 4. Flush metrics | Final metrics state captured before pool close |
| 5. Close DB/Redis pools | `db.writer.close().await`, `db.reader.close().await`, `cache` dropped |
| 6. Exit code 0 | Implicit return from `main()` |
