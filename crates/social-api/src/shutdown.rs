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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_install_signal_handler_returns_token() {
        // install_signal_handler() spawns a background task and returns a CancellationToken.
        // We verify it returns a valid token and that it is not yet cancelled.
        let token = install_signal_handler();
        assert!(!token.is_cancelled());
    }

    #[tokio::test]
    async fn test_cancellation_token_cancel() {
        // Verify that a CancellationToken can be cancelled and the state is observable.
        // (This tests the underlying tokio_util behavior used by install_signal_handler)
        let token = install_signal_handler();
        assert!(!token.is_cancelled());

        // The signal handler task is running in the background — we can cancel the token
        // programmatically to test the cancellation path.
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn test_token_clone_shares_cancellation() {
        let token = install_signal_handler();
        let token_clone = token.clone();

        assert!(!token.is_cancelled());
        assert!(!token_clone.is_cancelled());

        token.cancel();

        assert!(token.is_cancelled());
        assert!(token_clone.is_cancelled());
    }

    #[tokio::test]
    async fn test_cancelled_future_resolves_immediately() {
        let token = install_signal_handler();
        token.cancel();

        // cancelled() should resolve immediately when already cancelled
        let completed = tokio::time::timeout(Duration::from_millis(100), token.cancelled())
            .await
            .is_ok();

        assert!(completed, "cancelled() should resolve immediately after cancel()");
    }
}
