use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::db::DbPools;

/// Spawn a background task that records DB connection pool metrics every 15 seconds.
///
/// Emits `social_api_db_pool_connections{pool, state}` gauges for both writer
/// and reader pools. Cancels cleanly when the shutdown token is triggered.
pub fn spawn_db_pool_metrics(
    db: DbPools,
    config: Config,
    shutdown_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("DB pool metrics task started");

        loop {
            tokio::select! {
                _ = shutdown_token.cancelled() => {
                    tracing::info!("DB pool metrics task stopping (shutdown)");
                    break;
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(15)) => {
                    record_pool_metrics(&db, config.db_max_connections);
                }
            }
        }
    })
}

fn record_pool_metrics(db: &DbPools, max_connections: u32) {
    let writer_total = db.writer.size();
    let writer_idle = db.writer.num_idle() as u32;
    let writer_active = writer_total.saturating_sub(writer_idle);

    let reader_total = db.reader.size();
    let reader_idle = db.reader.num_idle() as u32;
    let reader_active = reader_total.saturating_sub(reader_idle);

    metrics::gauge!(
        "social_api_db_pool_connections",
        "pool" => "writer", "state" => "active"
    )
    .set(writer_active as f64);

    metrics::gauge!(
        "social_api_db_pool_connections",
        "pool" => "writer", "state" => "idle"
    )
    .set(writer_idle as f64);

    metrics::gauge!(
        "social_api_db_pool_connections",
        "pool" => "writer", "state" => "max"
    )
    .set(max_connections as f64);

    metrics::gauge!(
        "social_api_db_pool_connections",
        "pool" => "reader", "state" => "active"
    )
    .set(reader_active as f64);

    metrics::gauge!(
        "social_api_db_pool_connections",
        "pool" => "reader", "state" => "idle"
    )
    .set(reader_idle as f64);

    metrics::gauge!(
        "social_api_db_pool_connections",
        "pool" => "reader", "state" => "max"
    )
    .set(max_connections as f64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    struct TestHarness {
        _scope: crate::test_containers::TestScope,
        db: DbPools,
    }

    /// Return DbPools connected to the shared Postgres container.
    async fn setup_db() -> TestHarness {
        let scope = crate::test_containers::isolated_scope().await;

        let mut config = crate::config::Config::new_for_test();
        config.database_url = scope.database_url.clone();
        config.read_database_url = scope.database_url.clone();

        let db = DbPools::from_config(&config).await.expect("db pools");
        TestHarness { _scope: scope, db }
    }

    // -------------------------------------------------------------------------
    // 1. record_pool_metrics does not panic with real pool
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_record_pool_metrics_does_not_panic() {
        let harness = setup_db().await;
        let db = harness.db;
        // Should complete without panicking regardless of whether a metrics
        // recorder is installed (metrics crate no-ops when none is registered).
        record_pool_metrics(&db, 5);
    }

    // -------------------------------------------------------------------------
    // 2. record_pool_metrics with different max_connections values
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_record_pool_metrics_various_max_connections() {
        let harness = setup_db().await;
        let db = harness.db;
        for max in [1, 5, 20, 100] {
            record_pool_metrics(&db, max);
        }
    }

    // -------------------------------------------------------------------------
    // 3. spawn_db_pool_metrics task starts and cancels cleanly
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_db_pool_metrics_cancels_on_shutdown() {
        let harness = setup_db().await;
        let db = harness.db;

        let config = crate::config::Config::new_for_test();
        let shutdown_token = CancellationToken::new();

        let handle = spawn_db_pool_metrics(db, config, shutdown_token.clone());

        // Signal shutdown immediately -- the 15s periodic sleep should never fire.
        shutdown_token.cancel();

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        assert!(
            result.is_ok(),
            "spawn_db_pool_metrics handle must complete within 5s after cancel"
        );
    }

    // -------------------------------------------------------------------------
    // 4. pool size and idle counts are non-negative integers
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_pool_size_values_are_sane() {
        let harness = setup_db().await;
        let db = harness.db;

        // Verify the pool size metrics are readable without overflow
        let writer_total = db.writer.size();
        let writer_idle = db.writer.num_idle() as u32;
        let writer_active = writer_total.saturating_sub(writer_idle);

        // active connections must not exceed total
        assert!(writer_active <= writer_total);

        let reader_total = db.reader.size();
        let reader_idle = db.reader.num_idle() as u32;
        let reader_active = reader_total.saturating_sub(reader_idle);

        assert!(reader_active <= reader_total);
    }
}
