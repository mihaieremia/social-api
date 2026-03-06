mod cache;
mod clients;
mod config;
mod db;
mod errors;
mod extractors;
mod grpc;
mod handlers;
mod logging;
mod middleware;
mod openapi;
mod proto;
mod repositories;
mod server;
mod services;
mod shutdown;
mod state;
mod tasks;

#[cfg(test)]
mod test_containers;

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
    )
    .await;

    // ── 4. Spawn background tasks (cancellation-aware) ──
    let refresh_handle = tasks::leaderboard_refresh::spawn_leaderboard_refresh(
        db.clone(),
        cache.clone(),
        config.clone(),
        shutdown_token.clone(),
    );

    let pool_metrics_handle = tasks::db_pool_metrics::spawn_db_pool_metrics(
        db.clone(),
        config.clone(),
        shutdown_token.clone(),
    );

    // ── 5. Build router and bind HTTP ──
    let app = server::build_router(app_state.clone(), metrics_handle);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.http_port));
    let listener = TcpListener::bind(addr)
        .await
        .expect("Failed to bind HTTP address");

    tracing::info!(port = config.http_port, "HTTP server listening");

    // ── 5b. Build and bind gRPC server ──
    let grpc_server = server::build_grpc_server(app_state.clone());
    let grpc_addr = SocketAddr::from(([0, 0, 0, 0], config.grpc_port));

    tracing::info!(port = config.grpc_port, "gRPC server listening");

    // ── 6. Serve both HTTP and gRPC with graceful shutdown ──
    let shutdown_token_http = shutdown_token.clone();
    let shutdown_token_grpc = shutdown_token.clone();

    let http_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_token_http.cancelled_owned())
            .await
            .expect("HTTP server error");
    });

    let grpc_handle = tokio::spawn(async move {
        grpc_server
            .serve_with_shutdown(grpc_addr, shutdown_token_grpc.cancelled_owned())
            .await
            .expect("gRPC server error");
    });

    // Wait for both servers to stop
    let _ = tokio::join!(http_handle, grpc_handle);

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
        tracing::debug!(
            inflight = count,
            "Waiting for in-flight requests to drain..."
        );
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

    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), pool_metrics_handle).await;

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
