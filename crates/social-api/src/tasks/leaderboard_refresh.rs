use chrono::{Duration, Utc};
use futures::future::join_all;
use shared::types::TimeWindow;
use tokio_util::sync::CancellationToken;

use crate::cache::manager::CacheManager;
use crate::config::Config;
use crate::db::DbPools;
use crate::repositories::like_repository;

/// All time windows the refresh task iterates over.
const WINDOWS: [TimeWindow; 4] = [
    TimeWindow::Day,
    TimeWindow::Week,
    TimeWindow::Month,
    TimeWindow::All,
];

/// Maximum number of leaderboard entries stored per window.
const LEADERBOARD_LIMIT: i64 = 50;

/// Spawn a background task that periodically refreshes leaderboard sorted sets
/// in Redis so the leaderboard endpoint can read from cache instead of the DB.
///
/// Cancels cleanly when the shutdown token is triggered.
pub fn spawn_leaderboard_refresh(
    db: DbPools,
    cache: CacheManager,
    config: Config,
    shutdown_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let interval_secs = config.leaderboard_refresh_interval_secs;

    tokio::spawn(async move {
        tracing::info!(
            interval_secs = interval_secs,
            "Leaderboard refresh task started"
        );

        // Warm cache immediately on startup (avoids cold-start DB hammering)
        if let Err(e) = refresh_all_windows(&db, &cache).await {
            tracing::error!(error = %e, "Initial leaderboard refresh failed");
        }

        loop {
            tokio::select! {
                _ = shutdown_token.cancelled() => {
                    tracing::info!("Leaderboard refresh task stopping (shutdown)");
                    break;
                }
                _ = tokio::time::sleep({
                    // Add ±10% jitter so replicas don't all fire simultaneously.
                    let jitter_factor = {
                        let nanos = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .subsec_nanos();
                        let percent = (nanos % 201) as i64 - 100; // -100..+100
                        percent as f64 / 1000.0 // -0.1..+0.1
                    };
                    let sleep_secs = (interval_secs as f64 * (1.0 + jitter_factor)).max(1.0) as u64;
                    std::time::Duration::from_secs(sleep_secs)
                }) => {
                    if let Err(e) = refresh_all_windows(&db, &cache).await {
                        tracing::error!(error = %e, "Leaderboard refresh cycle failed");
                    }
                }
            }
        }
    })
}

/// Run one full refresh cycle across every time window — all windows in parallel.
async fn refresh_all_windows(
    db: &DbPools,
    cache: &CacheManager,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let futures: Vec<_> = WINDOWS
        .iter()
        .map(|&window| refresh_window(db, cache, window))
        .collect();

    let results = join_all(futures).await;
    for result in results {
        if let Err(e) = result {
            tracing::warn!(error = %e, "Failed to refresh leaderboard window");
        }
    }
    Ok(())
}

/// Refresh a single time window: query the DB and write the result into a
/// Redis sorted set keyed as `lb:{window}`.
async fn refresh_window(
    db: &DbPools,
    cache: &CacheManager,
    window: TimeWindow,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let since = window
        .duration_secs()
        .map(|secs| Utc::now() - Duration::seconds(secs));

    let rows = like_repository::get_leaderboard(&db.reader, None, since, LEADERBOARD_LIMIT)
        .await
        .map_err(|e| format!("DB query failed for window {window}: {e}"))?;

    let key = format!("lb:{}", window.as_str());

    // Build member/score pairs for ZADD.
    let mut members = Vec::with_capacity(rows.len());
    for (ct, cid, count) in &rows {
        members.push((format!("{ct}:{cid}"), *count as f64));
    }

    // Atomically replace the sorted set: DEL + ZADD in a pipeline.
    cache.replace_sorted_set(&key, &members).await;

    tracing::info!(
        window = window.as_str(),
        count = members.len(),
        "Leaderboard window refreshed"
    );

    Ok(())
}
