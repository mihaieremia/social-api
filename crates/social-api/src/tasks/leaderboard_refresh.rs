use chrono::{Duration, Utc};
use shared::types::TimeWindow;

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
/// The returned `JoinHandle` keeps the task alive; dropping it will **not**
/// cancel the task (tokio tasks are detached by default), but the caller can
/// use the handle to `.abort()` on shutdown if desired.
pub fn spawn_leaderboard_refresh(
    db: DbPools,
    cache: CacheManager,
    config: Config,
) -> tokio::task::JoinHandle<()> {
    let interval_secs = config.leaderboard_refresh_interval_secs;

    tokio::spawn(async move {
        tracing::info!(
            interval_secs = interval_secs,
            "Leaderboard refresh task started"
        );

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;

            if let Err(e) = refresh_all_windows(&db, &cache).await {
                tracing::error!(error = %e, "Leaderboard refresh cycle failed");
            }
        }
    })
}

/// Run one full refresh cycle across every time window.
async fn refresh_all_windows(
    db: &DbPools,
    cache: &CacheManager,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for window in WINDOWS {
        if let Err(e) = refresh_window(db, cache, window).await {
            tracing::warn!(
                window = window.as_str(),
                error = %e,
                "Failed to refresh leaderboard window"
            );
            // Continue with remaining windows — don't abort the whole cycle.
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
    let members: Vec<(String, f64)> = rows
        .iter()
        .map(|(ct, cid, count)| {
            let member = format!("{ct}:{cid}");
            (*count as f64, member)
        })
        .map(|(score, member)| (member, score))
        .collect();

    // Atomically replace the sorted set: DEL + ZADD in a pipeline.
    cache.replace_sorted_set(&key, &members).await;

    tracing::info!(
        window = window.as_str(),
        count = members.len(),
        "Leaderboard window refreshed"
    );

    Ok(())
}
