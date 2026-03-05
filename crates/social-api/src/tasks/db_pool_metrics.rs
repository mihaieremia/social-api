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
