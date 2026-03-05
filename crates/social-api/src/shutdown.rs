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
