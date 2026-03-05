mod cache;
mod config;
mod db;
mod handlers;
mod logging;
mod server;
mod shutdown;
mod state;

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

    // Initialize database pools
    let db = DbPools::from_config(&config)
        .await
        .expect("Failed to initialize database pools");

    // Run migrations
    db.run_migrations()
        .await
        .expect("Failed to run database migrations");

    // Initialize Redis pool
    let redis_pool = create_redis_pool(&config)
        .await
        .expect("Failed to initialize Redis pool");
    let cache = CacheManager::new(redis_pool);

    // Build application state
    let app_state = AppState::new(db, cache, config.clone());

    // Build router
    let app = server::build_router(app_state);

    // Bind listener
    let addr = SocketAddr::from(([0, 0, 0, 0], config.http_port));
    let listener = TcpListener::bind(addr)
        .await
        .expect("Failed to bind to address");

    tracing::info!(port = config.http_port, "Social API listening");

    // Serve with graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown::wait_for_signal())
        .await
        .expect("Server error");

    // Post-shutdown cleanup
    tracing::info!("Draining connections...");
    tokio::time::sleep(Duration::from_secs(1)).await;
    tracing::info!("Social API stopped");
}
