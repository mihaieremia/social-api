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

    // ── 7. Drain in-flight requests (with timeout) ──
    tracing::info!("Server stopped accepting connections, draining in-flight requests...");

    let drain_deadline =
        tokio::time::Instant::now() + Duration::from_secs(config.shutdown_timeout_secs);

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

    // ── 8. Wait for background tasks to exit (use remaining shutdown budget) ──
    tracing::info!("Waiting for background tasks to stop...");
    let remaining = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
    let task_timeout = remaining.max(Duration::from_secs(1)); // at least 1s
    match tokio::time::timeout(task_timeout, refresh_handle).await {
        Ok(Ok(())) => tracing::info!("Leaderboard refresh task stopped"),
        Ok(Err(e)) => tracing::warn!(error = %e, "Leaderboard refresh task panicked"),
        Err(_) => tracing::warn!("Leaderboard refresh task did not stop in time"),
    }

    // ── 9. Final metrics snapshot ──
    tracing::info!("Final metrics snapshot captured");

    // ── 10. Close database pools explicitly ──
    tracing::info!("Closing database pools...");
    db.writer.close().await;
    db.reader.close().await;
    tracing::info!("Database pools closed");

    // ── 11. Redis pool: bb8 drops idle connections when CacheManager goes out of scope ──

    tracing::info!("Social API stopped gracefully");
}
