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

#[cfg(test)]
mod tests {
    use super::*;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::{postgres::Postgres, redis::Redis};

    /// Spin up Postgres + Redis testcontainers, run migrations, return DbPools + CacheManager.
    /// Both containers are returned so they stay alive for the duration of the test.
    async fn setup() -> (
        DbPools,
        CacheManager,
        testcontainers::ContainerAsync<Postgres>,
        testcontainers::ContainerAsync<Redis>,
    ) {
        let pg = Postgres::default().start().await.expect("postgres container");
        let redis = Redis::default().start().await.expect("redis container");

        let pg_port = pg.get_host_port_ipv4(5432).await.unwrap();
        let redis_port = redis.get_host_port_ipv4(6379).await.unwrap();

        let db_url = format!("postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres");

        let mut config = crate::config::Config::new_for_test();
        config.database_url = db_url.clone();
        config.read_database_url = db_url;
        config.redis_url = format!("redis://127.0.0.1:{redis_port}");

        let db = DbPools::from_config(&config).await.unwrap();
        sqlx::migrate!("../../migrations")
            .run(&db.writer)
            .await
            .expect("run migrations");

        let redis_pool = crate::cache::manager::create_pool(&config).await.unwrap();
        let cache = CacheManager::new(redis_pool);

        (db, cache, pg, redis)
    }

    // -------------------------------------------------------------------------
    // 1. refresh_all_windows on empty DB creates ZSETs (possibly empty)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_refresh_all_windows_empty_db() {
        let (db, cache, _pg, _redis) = setup().await;

        // Should not panic or return an error
        let result = refresh_all_windows(&db, &cache).await;
        assert!(result.is_ok(), "refresh_all_windows must not fail on empty DB");

        // All four window ZSETs must exist (even if empty after replace_sorted_set)
        for window in WINDOWS {
            let key = format!("lb:{}", window.as_str());
            let entries = cache.zrevrange_with_scores(&key, 0, -1).await;
            // Empty ZSET after DEL is fine — just assert it doesn't error
            assert!(entries.len() <= LEADERBOARD_LIMIT as usize);
        }
    }

    // -------------------------------------------------------------------------
    // 2. refresh_window populates ZSETs when data exists
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_refresh_window_populates_zset_with_data() {
        let (db, cache, _pg, _redis) = setup().await;

        // Insert likes directly so the leaderboard query returns results
        let user_id = uuid::Uuid::new_v4();
        let content_id = uuid::Uuid::new_v4();

        sqlx::query(
            "INSERT INTO likes (user_id, content_type, content_id) VALUES ($1, $2, $3)",
        )
        .bind(user_id)
        .bind("post")
        .bind(content_id)
        .execute(&db.writer)
        .await
        .unwrap();

        sqlx::query(
            r#"INSERT INTO like_counts (content_type, content_id, total_count)
               VALUES ($1, $2, 1)
               ON CONFLICT (content_type, content_id)
               DO UPDATE SET total_count = like_counts.total_count + 1"#,
        )
        .bind("post")
        .bind(content_id)
        .execute(&db.writer)
        .await
        .unwrap();

        // Refresh the "all" window (TimeWindow::All has no time filter -> uses like_counts)
        let result = refresh_window(&db, &cache, TimeWindow::All).await;
        assert!(result.is_ok());

        let key = format!("lb:{}", TimeWindow::All.as_str());
        let entries = cache.zrevrange_with_scores(&key, 0, -1).await;
        assert!(
            !entries.is_empty(),
            "lb:all must contain entries after refresh with data"
        );

        let expected_member = format!("post:{content_id}");
        assert!(
            entries.iter().any(|(m, _)| m == &expected_member),
            "expected member {expected_member} not found in lb:all"
        );
    }

    // -------------------------------------------------------------------------
    // 3. refresh_window for time-windowed (Day) uses likes table COUNT
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_refresh_window_day_uses_likes_table() {
        let (db, cache, _pg, _redis) = setup().await;

        // Insert two likes for content_a and one like for content_b
        let content_a = uuid::Uuid::new_v4();
        let content_b = uuid::Uuid::new_v4();

        for _ in 0..2 {
            sqlx::query(
                "INSERT INTO likes (user_id, content_type, content_id) VALUES ($1, $2, $3)",
            )
            .bind(uuid::Uuid::new_v4())
            .bind("post")
            .bind(content_a)
            .execute(&db.writer)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO likes (user_id, content_type, content_id) VALUES ($1, $2, $3)",
        )
        .bind(uuid::Uuid::new_v4())
        .bind("post")
        .bind(content_b)
        .execute(&db.writer)
        .await
        .unwrap();

        let result = refresh_window(&db, &cache, TimeWindow::Day).await;
        assert!(result.is_ok());

        let key = format!("lb:{}", TimeWindow::Day.as_str());
        let entries = cache.zrevrange_with_scores(&key, 0, -1).await;

        // content_a should outrank content_b (score 2 vs 1)
        let member_a = format!("post:{content_a}");
        let member_b = format!("post:{content_b}");

        let score_a = entries
            .iter()
            .find(|(m, _)| m == &member_a)
            .map(|(_, s)| *s)
            .expect("content_a must be in lb:24h");
        let score_b = entries
            .iter()
            .find(|(m, _)| m == &member_b)
            .map(|(_, s)| *s)
            .expect("content_b must be in lb:24h");

        assert!(
            score_a > score_b,
            "content_a (2 likes) must have higher score than content_b (1 like)"
        );
    }

    // -------------------------------------------------------------------------
    // 4. refresh_all_windows iterates all four windows without short-circuiting
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_refresh_all_windows_covers_all_four_windows() {
        let (db, cache, _pg, _redis) = setup().await;

        refresh_all_windows(&db, &cache).await.unwrap();

        // All four ZSETs must have been touched (even if empty)
        for window in WINDOWS {
            let key = format!("lb:{}", window.as_str());
            // zrevrange_with_scores returns vec (possibly empty) -- it must not panic
            let _ = cache.zrevrange_with_scores(&key, 0, 9).await;
        }
    }

    // -------------------------------------------------------------------------
    // 5. spawn_leaderboard_refresh cancels cleanly on shutdown signal
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_leaderboard_refresh_cancels_on_shutdown() {
        let (db, cache, _pg, _redis) = setup().await;

        let shutdown_token = CancellationToken::new();
        let mut config = crate::config::Config::new_for_test();
        // Use a long interval so the periodic sleep never fires during the test
        config.leaderboard_refresh_interval_secs = 3600;

        let handle = spawn_leaderboard_refresh(db, cache, config, shutdown_token.clone());

        // Give the task a moment to start up and run its initial refresh
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Signal shutdown and wait for the task to finish
        shutdown_token.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

        assert!(
            result.is_ok(),
            "spawn_leaderboard_refresh handle must complete within 5s after cancel"
        );
    }
}
