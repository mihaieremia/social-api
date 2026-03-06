//! Leaderboard refresh stress test.
//!
//! Seeds a configurable volume of likes across many content items, then
//! benchmarks `refresh_all_windows` and individual window refreshes to
//! measure wall-clock latency, DB query cost, and Redis write throughput.
//!
//! **Requires Docker** (testcontainers for Postgres + Redis).
//!
//! Run with defaults (10K likes, 500 content items):
//!   cargo test --test leaderboard_stress_test -- --ignored --nocapture
//!
//! Run with custom volume:
//!   STRESS_LIKES=100000 STRESS_CONTENT_ITEMS=2000 \
//!     cargo test --test leaderboard_stress_test -- --ignored --nocapture
//!
//! Environment variables:
//!   STRESS_LIKES          — Total likes to seed (default: 10_000)
//!   STRESS_CONTENT_ITEMS  — Distinct content items (default: 500)
//!   STRESS_CONTENT_TYPES  — Number of content types (default: 3)
//!   STRESS_REFRESH_RUNS   — How many refresh cycles to time (default: 5)

use social_api::cache::manager::{CacheManager, create_pool};
use social_api::config::Config;
use social_api::db::DbPools;
use social_api::tasks::leaderboard_refresh;

use std::time::Instant;
use testcontainers::runners::AsyncRunner;
use tokio::sync::OnceCell;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Infrastructure setup (own containers — integration tests can't use #[cfg(test)] modules)
// ---------------------------------------------------------------------------

struct SharedContainers {
    _pg: testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    _redis: testcontainers::ContainerAsync<testcontainers_modules::redis::Redis>,
    db_url: String,
    redis_url: String,
}

static CONTAINERS: OnceCell<SharedContainers> = OnceCell::const_new();

async fn shared_containers() -> &'static SharedContainers {
    CONTAINERS
        .get_or_init(|| async {
            let pg = testcontainers_modules::postgres::Postgres::default()
                .start()
                .await
                .expect("postgres container");
            let pg_port = pg.get_host_port_ipv4(5432).await.unwrap();
            let db_url = format!("postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres");

            let redis = testcontainers_modules::redis::Redis::default()
                .start()
                .await
                .expect("redis container");
            let redis_port = redis.get_host_port_ipv4(6379).await.unwrap();
            let redis_url = format!("redis://127.0.0.1:{redis_port}");

            // Run migrations once
            let pool = sqlx::PgPool::connect(&db_url).await.expect("pg connect");
            sqlx::migrate!("../../migrations")
                .run(&pool)
                .await
                .expect("migrations");
            pool.close().await;

            SharedContainers {
                _pg: pg,
                _redis: redis,
                db_url,
                redis_url,
            }
        })
        .await
}

async fn setup() -> (DbPools, CacheManager) {
    let containers = shared_containers().await;

    let mut config = Config::new_for_test();
    config.database_url = containers.db_url.clone();
    config.read_database_url = containers.db_url.clone();
    config.redis_url = containers.redis_url.clone();

    let db = DbPools::from_config(&config).await.unwrap();
    let redis_pool = create_pool(&config).await.unwrap();
    let cache = CacheManager::new(redis_pool);

    (db, cache)
}

fn env_or(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Data seeding
// ---------------------------------------------------------------------------

const CONTENT_TYPE_NAMES: &[&str] = &[
    "post",
    "bonus_hunter",
    "top_picks",
    "article",
    "video",
    "podcast",
];

/// Seed `total_likes` likes spread across `num_items` content items
/// using `num_types` content types. Also populates `like_counts` so the
/// all-time leaderboard (which reads from `like_counts`) works correctly.
///
/// Uses batch inserts for speed (1000 rows per INSERT).
async fn seed_likes(
    db: &DbPools,
    total_likes: usize,
    num_items: usize,
    num_types: usize,
) -> Vec<(String, Uuid)> {
    let types: Vec<&str> = CONTENT_TYPE_NAMES.iter().take(num_types).copied().collect();

    // Generate content items
    let items: Vec<(String, Uuid)> = (0..num_items)
        .map(|i| {
            let ct = types[i % types.len()].to_string();
            let cid = Uuid::new_v4();
            (ct, cid)
        })
        .collect();

    eprintln!(
        "  Seeding {} likes across {} content items ({} types)...",
        total_likes, num_items, num_types
    );

    let seed_start = Instant::now();

    // Batch insert likes (1000 per statement for speed)
    const BATCH_SIZE: usize = 1000;
    let mut inserted = 0;

    while inserted < total_likes {
        let batch_end = (inserted + BATCH_SIZE).min(total_likes);
        let batch_count = batch_end - inserted;

        let mut query =
            String::from("INSERT INTO likes (user_id, content_type, content_id) VALUES ");

        let mut param_idx = 1;
        for i in 0..batch_count {
            if i > 0 {
                query.push_str(", ");
            }
            query.push_str(&format!(
                "(${}, ${}, ${})",
                param_idx,
                param_idx + 1,
                param_idx + 2
            ));
            param_idx += 3;
        }

        let mut q = sqlx::query(&query);
        for j in 0..batch_count {
            let idx = inserted + j;
            let (ct, cid) = &items[idx % items.len()];
            q = q.bind(Uuid::new_v4()).bind(ct.as_str()).bind(cid);
        }

        q.execute(&db.writer).await.unwrap();
        inserted = batch_end;
    }

    let seed_likes_elapsed = seed_start.elapsed();
    eprintln!(
        "  Seeded {} likes in {:.2}s ({:.0} rows/sec)",
        total_likes,
        seed_likes_elapsed.as_secs_f64(),
        total_likes as f64 / seed_likes_elapsed.as_secs_f64()
    );

    // Populate like_counts from the likes table
    let counts_start = Instant::now();
    sqlx::query(
        r#"
        INSERT INTO like_counts (content_type, content_id, total_count)
        SELECT content_type, content_id, COUNT(*)
        FROM likes
        GROUP BY content_type, content_id
        ON CONFLICT (content_type, content_id)
        DO UPDATE SET total_count = EXCLUDED.total_count
        "#,
    )
    .execute(&db.writer)
    .await
    .unwrap();

    eprintln!(
        "  Populated like_counts in {:.2}s",
        counts_start.elapsed().as_secs_f64()
    );
    eprintln!(
        "  Total seed time: {:.2}s",
        seed_start.elapsed().as_secs_f64()
    );

    items
}

// ---------------------------------------------------------------------------
// Stress test
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore] // Only runs when explicitly requested (--ignored)
async fn test_leaderboard_refresh_under_load() {
    let total_likes = env_or("STRESS_LIKES", 10_000);
    let num_items = env_or("STRESS_CONTENT_ITEMS", 500);
    let num_types = env_or("STRESS_CONTENT_TYPES", 3);
    let num_runs = env_or("STRESS_REFRESH_RUNS", 5);

    eprintln!("\n=== Leaderboard Refresh Stress Test ===");
    eprintln!(
        "  Config: {} likes, {} items, {} types, {} runs",
        total_likes, num_items, num_types, num_runs
    );

    let (db, cache) = setup().await;
    let _items = seed_likes(&db, total_likes, num_items, num_types).await;

    // Verify data was seeded correctly
    let (like_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM likes")
        .fetch_one(&db.reader)
        .await
        .unwrap();
    let (item_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM like_counts")
        .fetch_one(&db.reader)
        .await
        .unwrap();
    eprintln!(
        "\n  Verified: {} likes, {} distinct content items in DB",
        like_count, item_count
    );

    // --- Benchmark: refresh_all_windows (all 4 windows in parallel) ---
    eprintln!("\n--- refresh_all_windows (4 windows in parallel) ---");

    let mut all_durations = Vec::with_capacity(num_runs);
    for run in 0..num_runs {
        let start = Instant::now();
        leaderboard_refresh::refresh_all_windows_public(&db, &cache)
            .await
            .expect("refresh_all_windows must not fail");
        let elapsed = start.elapsed();
        all_durations.push(elapsed);
        eprintln!("  Run {}: {:.2}ms", run + 1, elapsed.as_secs_f64() * 1000.0);
    }

    let avg_ms =
        all_durations.iter().map(|d| d.as_secs_f64()).sum::<f64>() / num_runs as f64 * 1000.0;
    let max_ms = all_durations
        .iter()
        .map(|d| d.as_secs_f64() * 1000.0)
        .fold(0.0_f64, f64::max);
    let min_ms = all_durations
        .iter()
        .map(|d| d.as_secs_f64() * 1000.0)
        .fold(f64::MAX, f64::min);

    eprintln!("\n  Summary: avg={avg_ms:.2}ms  min={min_ms:.2}ms  max={max_ms:.2}ms");

    // --- Benchmark: individual windows ---
    eprintln!("\n--- Individual window refresh (single run each) ---");

    use shared::types::TimeWindow;
    let windows = [
        TimeWindow::Day,
        TimeWindow::Week,
        TimeWindow::Month,
        TimeWindow::All,
    ];

    for window in &windows {
        let start = Instant::now();
        leaderboard_refresh::refresh_window_public(&db, &cache, *window)
            .await
            .expect("refresh_window must not fail");
        let elapsed = start.elapsed();
        eprintln!(
            "  {:>5}: {:.2}ms",
            window.as_str(),
            elapsed.as_secs_f64() * 1000.0
        );
    }

    // --- Verify Redis ZSETs were populated ---
    eprintln!("\n--- Redis ZSET verification ---");
    for window in &windows {
        let key = format!("lb:{}", window.as_str());
        let entries = cache.zrevrange_with_scores(&key, 0, -1).await;
        eprintln!("  lb:{}: {} entries", window.as_str(), entries.len());
        assert!(
            !entries.is_empty(),
            "lb:{} must not be empty after refresh with {} likes",
            window.as_str(),
            total_likes
        );
    }

    // --- Assertions ---
    // With 10K likes, refresh should complete well under 5s even on slow CI.
    // Scale threshold linearly for larger data sets.
    let threshold_ms = 5000.0 + (total_likes as f64 / 10_000.0) * 2000.0;
    assert!(
        max_ms < threshold_ms,
        "Worst-case refresh ({max_ms:.0}ms) exceeded threshold ({threshold_ms:.0}ms)"
    );

    eprintln!("\n=== PASS (threshold: {threshold_ms:.0}ms) ===\n");
}
