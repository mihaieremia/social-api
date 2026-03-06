mod content;
mod data;
mod grpc_content;
mod grpc_profile;
mod profile;
mod proto;

use axum::{Router, routing::get};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use proto::internal_v1::content_service_server::ContentServiceServer;
use proto::internal_v1::profile_service_server::ProfileServiceServer;

#[tokio::main]
async fn main() {
    // Init logging
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().json())
        .init();

    // Single router serving all mock HTTP APIs.
    // Docker Compose maps multiple service names to this container.
    // Content API: GET /v1/{content_type}/{content_id}
    // Profile API: GET /v1/auth/validate
    let app = Router::new()
        .route("/v1/auth/validate", get(profile::validate_token))
        .route("/v1/{content_type}/{content_id}", get(content::get_content))
        .route("/health", get(content::health));

    let http_port: u16 = std::env::var("MOCK_PORT")
        .unwrap_or_else(|_| "8081".to_string())
        .parse()
        .expect("MOCK_PORT must be a valid port");

    let grpc_port: u16 = std::env::var("MOCK_GRPC_PORT")
        .unwrap_or_else(|_| "50052".to_string())
        .parse()
        .expect("MOCK_GRPC_PORT must be a valid port");

    // Spawn HTTP server
    let http_addr = SocketAddr::from(([0, 0, 0, 0], http_port));
    tracing::info!("Mock HTTP services listening on {}", http_addr);
    let listener = TcpListener::bind(http_addr)
        .await
        .expect("Failed to bind HTTP");

    let http_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("HTTP server failed");
    });

    // Spawn gRPC server
    let grpc_addr = SocketAddr::from(([0, 0, 0, 0], grpc_port));
    tracing::info!("Mock gRPC services listening on {}", grpc_addr);

    let grpc_handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(ContentServiceServer::new(grpc_content::MockContentService))
            .add_service(ProfileServiceServer::new(grpc_profile::MockProfileService))
            .serve(grpc_addr)
            .await
            .expect("gRPC server failed");
    });

    // Exit if either server task completes (which indicates a failure)
    tokio::select! {
        _ = http_handle => {},
        _ = grpc_handle => {},
    }
}
