use tokio::signal;

/// Wait for SIGTERM or SIGINT, then return.
/// Used for graceful shutdown: stop accepting, drain in-flight, close pools.
pub async fn wait_for_signal() {
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
}
