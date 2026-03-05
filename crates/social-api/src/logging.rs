use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize structured JSON logging.
///
/// Uses `RUST_LOG` env var for filtering (e.g., `social_api=debug,tower_http=debug`).
/// Falls back to the provided `default_level` if `RUST_LOG` is not set.
///
/// All log lines are JSON with: timestamp, level, message, target, span fields.
/// Request-scoped fields (request_id, method, path) are added via tracing spans
/// in the middleware layer.
pub fn init(default_level: &str) {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("social_api={default_level},tower_http=info")));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::layer()
                .json()
                .with_target(true)
                .with_thread_ids(false)
                .with_file(false)
                .with_line_number(false)
                .with_current_span(true)
                .with_span_list(false),
        )
        .init();
}
