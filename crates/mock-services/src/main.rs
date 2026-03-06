mod content;
mod data;
mod profile;
mod proto;

use axum::{Router, routing::get};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() {
    // Init logging
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().json())
        .init();

    // Single router serving all mock APIs.
    // Docker Compose maps multiple service names to this container.
    // Content API: GET /v1/{content_type}/{content_id}
    // Profile API: GET /v1/auth/validate
    let app = Router::new()
        .route("/v1/auth/validate", get(profile::validate_token))
        .route("/v1/{content_type}/{content_id}", get(content::get_content))
        .route("/health", get(content::health));

    let port: u16 = std::env::var("MOCK_PORT")
        .unwrap_or_else(|_| "8081".to_string())
        .parse()
        .expect("MOCK_PORT must be a valid port");

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("Mock services listening on {}", addr);

    let listener = TcpListener::bind(addr).await.expect("Failed to bind");
    axum::serve(listener, app).await.expect("Server failed");
}
