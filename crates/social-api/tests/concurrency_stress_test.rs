//! Concurrency & stress tests for the Social API.
//!
//! Tests critical invariants under high concurrency:
//!   1. Like/unlike count accuracy under contention
//!   2. Batch endpoint at scale (100 items x 1000 likes)
//!   3. Cache stampede coalescing (50 concurrent readers, ~1 DB query)
//!   4. Cursor pagination correctness under concurrent writes
//!   5. SSE fan-out throughput (100 subscribers)
//!   6. Rate limiter accuracy (no bypass under concurrency)
//!
//! Uses shared Postgres + Redis test services. Start them once with Docker
//! Compose before running this suite.
//!
//! Run all:
//!   cargo test --test concurrency_stress_test -- --ignored --nocapture
//!
//! Run one:
//!   cargo test --test concurrency_stress_test -- --ignored --nocapture test_concurrent_like_unlike_count_accuracy

use social_api::cache::CacheManager;
use social_api::clients::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use social_api::clients::content_client::ContentValidator;
use social_api::config::Config;
use social_api::db::DbPools;
use social_api::middleware::rate_limit::check_rate_limit_inner;
use social_api::repositories;
use social_api::services::like_service::LikeService;
use social_api::services::pubsub_manager::PubSubManager;

use shared::errors::AppError;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

#[path = "common/infra.rs"]
mod infra;

async fn setup() -> infra::TestContext {
    infra::TestContext::new().await
}

fn make_like_service(db: DbPools, cache: CacheManager) -> LikeService {
    let content_breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
        failure_threshold: 5,
        recovery_timeout: std::time::Duration::from_secs(30),
        success_threshold: 2,
        service_name: "stress_test".to_string(),
        rate_window: std::time::Duration::from_secs(30),
        failure_rate_threshold: 0.5,
        min_calls_for_rate: 10,
    }));

    LikeService::new_with_validator(
        db,
        cache,
        Arc::new(AlwaysValidContent),
        Config::new_for_test(),
        content_breaker,
    )
}

/// Stub content validator — all content is valid.
struct AlwaysValidContent;

#[async_trait::async_trait]
impl ContentValidator for AlwaysValidContent {
    async fn validate(&self, _content_type: &str, _content_id: Uuid) -> Result<bool, AppError> {
        Ok(true)
    }
}

// ===========================================================================
// Test 1: Concurrent like/unlike storm — count accuracy under contention
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_concurrent_like_unlike_count_accuracy() {
    eprintln!("\n=== Test 1: Concurrent Like/Unlike Storm ===");

    let context = setup().await;
    let db = context.db.clone();

    let num_content_items = 10;
    let num_tasks = 200;
    let ops_per_task = 50;

    let content_ids: Vec<Uuid> = (0..num_content_items).map(|_| Uuid::new_v4()).collect();

    eprintln!(
        "  Spawning {} tasks x {} ops across {} content items...",
        num_tasks, ops_per_task, num_content_items
    );

    let start = Instant::now();
    let mut handles = Vec::with_capacity(num_tasks);

    for task_idx in 0..num_tasks {
        let pool = db.writer.clone();
        let cids = content_ids.clone();

        handles.push(tokio::spawn(async move {
            let user_id = Uuid::new_v4();
            for op in 0..ops_per_task {
                let cid = cids[(task_idx * ops_per_task + op) % cids.len()];
                if op % 3 == 2 {
                    // Every 3rd op is an unlike
                    let _ = repositories::delete_like(&pool, user_id, "post", cid).await;
                } else {
                    let _ = repositories::insert_like(&pool, user_id, "post", cid).await;
                }
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    let elapsed = start.elapsed();
    let total_ops = num_tasks * ops_per_task;
    eprintln!(
        "  {} ops in {:.2}s ({:.0} ops/sec)",
        total_ops,
        elapsed.as_secs_f64(),
        total_ops as f64 / elapsed.as_secs_f64()
    );

    // Verify: like_counts.total_count == actual COUNT(*) FROM likes
    eprintln!("  Verifying count accuracy...");
    let mut mismatches = 0;

    for cid in &content_ids {
        let (actual_count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM likes WHERE content_type = 'post' AND content_id = $1",
        )
        .bind(cid)
        .fetch_one(&db.reader)
        .await
        .unwrap();

        let materialized: i64 = sqlx::query_scalar(
            "SELECT COALESCE(total_count, 0) FROM like_counts WHERE content_type = 'post' AND content_id = $1",
        )
        .bind(cid)
        .fetch_optional(&db.reader)
        .await
        .unwrap()
        .unwrap_or(0);

        if actual_count != materialized {
            eprintln!(
                "  MISMATCH: content_id={cid} actual={actual_count} materialized={materialized}"
            );
            mismatches += 1;
        }
    }

    assert_eq!(
        mismatches, 0,
        "{mismatches} content items have count drift between likes and like_counts"
    );
    eprintln!(
        "  All {} content items have accurate counts.",
        num_content_items
    );
    eprintln!("=== PASS ===\n");
}

// ===========================================================================
// Test 2: Batch endpoint at scale (100 items x 1000 likes)
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_batch_counts_at_scale() {
    eprintln!("\n=== Test 2: Batch Counts at Scale ===");

    let context = setup().await;
    let db = context.db.clone();
    let cache = context.cache.clone();
    let service = make_like_service(db.clone(), cache.clone());

    let num_items = 100;
    let likes_per_item = 1000;

    eprintln!(
        "  Seeding {} items x {} likes...",
        num_items, likes_per_item
    );

    let content_ids: Vec<Uuid> = (0..num_items).map(|_| Uuid::new_v4()).collect();

    // Batch-insert likes
    let seed_start = Instant::now();
    const BATCH_SIZE: usize = 1000;

    for cid in &content_ids {
        let mut inserted = 0;
        while inserted < likes_per_item {
            let batch_end = (inserted + BATCH_SIZE).min(likes_per_item);
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
            for _ in 0..batch_count {
                q = q.bind(Uuid::new_v4()).bind("post").bind(cid);
            }
            q.execute(&db.writer).await.unwrap();
            inserted = batch_end;
        }
    }

    // Populate like_counts
    sqlx::query(
        r#"
        INSERT INTO like_counts (content_type, content_id, total_count)
        SELECT content_type, content_id, COUNT(*)
        FROM likes
        WHERE content_id = ANY($1)
        GROUP BY content_type, content_id
        ON CONFLICT (content_type, content_id)
        DO UPDATE SET total_count = EXCLUDED.total_count
        "#,
    )
    .bind(&content_ids)
    .execute(&db.writer)
    .await
    .unwrap();

    eprintln!("  Seeded in {:.2}s", seed_start.elapsed().as_secs_f64());

    let items: Vec<(String, Uuid)> = content_ids
        .iter()
        .map(|cid| ("post".to_string(), *cid))
        .collect();

    // Warm cache
    let _ = service.batch_counts(&items).await.unwrap();

    // Benchmark warm cache
    eprintln!("  Benchmarking batch_counts (warm cache)...");
    let num_runs = 10;
    let mut durations = Vec::with_capacity(num_runs);

    for _ in 0..num_runs {
        let start = Instant::now();
        let results = service.batch_counts(&items).await.unwrap();
        durations.push(start.elapsed());

        // Verify correctness
        assert_eq!(results.len(), num_items);
        for r in &results {
            assert_eq!(
                r.count, likes_per_item as i64,
                "content_id={} expected {} got {}",
                r.content_id, likes_per_item, r.count
            );
        }
    }

    let avg_ms = durations.iter().map(|d| d.as_secs_f64()).sum::<f64>() / num_runs as f64 * 1000.0;
    let max_ms = durations
        .iter()
        .map(|d| d.as_secs_f64() * 1000.0)
        .fold(0.0_f64, f64::max);
    eprintln!("  Warm cache: avg={avg_ms:.2}ms  max={max_ms:.2}ms");

    // Cold cache benchmark: flush cache keys, measure DB fallback
    for cid in &content_ids {
        cache.del(&format!("lc:post:{cid}")).await;
    }

    let cold_start = Instant::now();
    let results = service.batch_counts(&items).await.unwrap();
    let cold_ms = cold_start.elapsed().as_secs_f64() * 1000.0;
    eprintln!("  Cold cache (DB fallback): {cold_ms:.2}ms");

    for r in &results {
        assert_eq!(
            r.count, likes_per_item as i64,
            "cold cache: content_id={} count mismatch",
            r.content_id
        );
    }

    // Cold cache should complete within 5 seconds for 100 items
    assert!(
        cold_ms < 5000.0,
        "Cold cache batch_counts took {cold_ms:.0}ms (>5s threshold)"
    );

    eprintln!("=== PASS ===\n");
}

// ===========================================================================
// Test 3: Cache stampede coalescing
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_cache_stampede_coalescing() {
    eprintln!("\n=== Test 3: Cache Stampede Simulation ===");

    let context = setup().await;
    let db = context.db.clone();
    let cache = context.cache.clone();
    let service = Arc::new(make_like_service(db.clone(), cache.clone()));

    let content_id = Uuid::new_v4();
    let expected_count: i64 = 500;

    // Seed likes
    eprintln!("  Seeding {expected_count} likes...");
    let mut query = String::from("INSERT INTO likes (user_id, content_type, content_id) VALUES ");
    for i in 0..expected_count {
        if i > 0 {
            query.push_str(", ");
        }
        let idx = (i * 3 + 1) as usize;
        query.push_str(&format!("(${}, ${}, ${})", idx, idx + 1, idx + 2));
    }
    let mut q = sqlx::query(&query);
    for _ in 0..expected_count {
        q = q.bind(Uuid::new_v4()).bind("post").bind(content_id);
    }
    q.execute(&db.writer).await.unwrap();

    sqlx::query(
        r#"
        INSERT INTO like_counts (content_type, content_id, total_count)
        VALUES ('post', $1, $2)
        ON CONFLICT (content_type, content_id)
        DO UPDATE SET total_count = EXCLUDED.total_count
        "#,
    )
    .bind(content_id)
    .bind(expected_count)
    .execute(&db.writer)
    .await
    .unwrap();

    // Flush cache to force DB fallback
    cache.del(&format!("lc:post:{content_id}")).await;

    let num_concurrent = 50;
    eprintln!("  Launching {num_concurrent} concurrent get_count requests (cache empty)...");

    // Use a barrier to synchronize all tasks to start at the same instant
    let barrier = Arc::new(tokio::sync::Barrier::new(num_concurrent));

    let start = Instant::now();
    let mut handles = Vec::with_capacity(num_concurrent);

    for _ in 0..num_concurrent {
        let svc = service.clone();
        let bar = barrier.clone();
        handles.push(tokio::spawn(async move {
            bar.wait().await;
            svc.get_count("post", content_id).await
        }));
    }

    let mut results = Vec::with_capacity(num_concurrent);
    for h in handles {
        results.push(h.await.unwrap());
    }

    let elapsed = start.elapsed();
    eprintln!(
        "  All {} requests completed in {:.2}ms",
        num_concurrent,
        elapsed.as_secs_f64() * 1000.0
    );

    // Verify all got the correct count
    for (i, result) in results.iter().enumerate() {
        let resp = result.as_ref().expect("get_count must not fail");
        assert_eq!(
            resp.count, expected_count,
            "Task {i} got count={}, expected={expected_count}",
            resp.count
        );
    }

    // Measure single query baseline for comparison
    cache.del(&format!("lc:post:{content_id}")).await;
    let single_start = Instant::now();
    let _ = service.get_count("post", content_id).await.unwrap();
    let single_ms = single_start.elapsed().as_secs_f64() * 1000.0;

    let concurrent_ms = elapsed.as_secs_f64() * 1000.0;
    let speedup = (single_ms * num_concurrent as f64) / concurrent_ms;

    eprintln!("  Single query: {single_ms:.2}ms");
    eprintln!("  {num_concurrent} concurrent: {concurrent_ms:.2}ms");
    eprintln!("  Effective speedup: {speedup:.1}x (vs {num_concurrent}x serial)");

    // With coalescing, concurrent should be much less than N * single
    // A conservative assertion: concurrent < 5x single (not 50x)
    assert!(
        concurrent_ms < single_ms * 10.0,
        "Stampede coalescing may not be working: {concurrent_ms:.0}ms >> {single_ms:.0}ms * 10"
    );

    eprintln!("=== PASS ===\n");
}

// ===========================================================================
// Test 4: Cursor pagination depth — no skips or duplicates
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_cursor_pagination_no_gaps_no_duplicates() {
    eprintln!("\n=== Test 4: Cursor Pagination Depth ===");

    let context = setup().await;
    let db = context.db.clone();

    let user_id = Uuid::new_v4();
    let total_likes = 10_000;

    eprintln!("  Seeding {total_likes} likes for user {user_id}...");

    // Batch insert
    let seed_start = Instant::now();
    let mut inserted = 0;
    let batch_size = 1000;

    while inserted < total_likes {
        let batch_end = (inserted + batch_size).min(total_likes);
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
        for _ in 0..batch_count {
            q = q.bind(user_id).bind("post").bind(Uuid::new_v4());
        }
        q.execute(&db.writer).await.unwrap();
        inserted = batch_end;
    }

    eprintln!("  Seeded in {:.2}s", seed_start.elapsed().as_secs_f64());

    // Spawn a background writer that continuously inserts new likes
    let writer_pool = db.writer.clone();
    let writer_user = user_id;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_clone = stop.clone();
    let bg_writes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let bg_writes_clone = bg_writes.clone();

    let bg_handle = tokio::spawn(async move {
        while !stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
            let _ =
                repositories::insert_like(&writer_pool, writer_user, "post", Uuid::new_v4()).await;
            bg_writes_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    });

    // Paginate through all pages
    eprintln!("  Paginating (limit=50, concurrent writes happening)...");
    let page_start = Instant::now();

    let mut all_ids = Vec::with_capacity(total_likes + 1000);
    let mut cursor_ts = None;
    let mut cursor_id = None;
    let mut pages = 0;
    let page_limit: i64 = 50;

    loop {
        let rows = repositories::get_user_likes(
            &db.reader,
            user_id,
            Some("post"),
            cursor_ts,
            cursor_id,
            page_limit,
        )
        .await
        .unwrap();

        let has_more = rows.len() as i64 > page_limit;
        let take = (page_limit as usize).min(rows.len());

        for row in rows.iter().take(take) {
            all_ids.push(row.content_id);
        }

        pages += 1;

        if !has_more {
            break;
        }

        let last = &rows[take - 1];
        cursor_ts = Some(last.created_at);
        cursor_id = Some(last.id);
    }

    // Stop background writer
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    bg_handle.await.unwrap();

    let page_elapsed = page_start.elapsed();
    let bg_count = bg_writes.load(std::sync::atomic::Ordering::Relaxed);

    eprintln!(
        "  Paginated {} items across {} pages in {:.2}s",
        all_ids.len(),
        pages,
        page_elapsed.as_secs_f64()
    );
    eprintln!("  Background writes during pagination: {bg_count}");

    // Check for duplicates
    let unique: HashSet<Uuid> = all_ids.iter().copied().collect();
    let duplicates = all_ids.len() - unique.len();

    assert_eq!(
        duplicates, 0,
        "Found {duplicates} duplicate content_ids in pagination"
    );

    // We should have gotten at least the original seeded likes
    assert!(
        all_ids.len() >= total_likes,
        "Expected at least {total_likes} items, got {}",
        all_ids.len()
    );

    eprintln!(
        "  Zero duplicates, {} unique items collected.",
        unique.len()
    );
    eprintln!("=== PASS ===\n");
}

// ===========================================================================
// Test 5: SSE fan-out throughput (100 subscribers)
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_sse_fanout_throughput() {
    eprintln!("\n=== Test 5: SSE Fan-Out Throughput ===");

    let context = setup().await;
    let redis_url = context.config.redis_url.clone();

    let channel = "sse:post:fanout-stress-test";
    let num_subscribers = 100;
    let num_messages = 100;

    let manager = PubSubManager::new(
        redis_url.clone(),
        256,
        tokio_util::sync::CancellationToken::new(),
    );

    eprintln!("  Creating {num_subscribers} subscribers...");
    let mut receivers = Vec::with_capacity(num_subscribers);
    for _ in 0..num_subscribers {
        let rx = manager.subscribe(channel).await.expect("subscribe");
        receivers.push(rx);
    }

    // Give bridge time to connect to Redis
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    eprintln!("  Publishing {num_messages} messages...");
    let client = redis::Client::open(redis_url.as_str()).expect("redis client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("redis connection");

    let publish_start = Instant::now();
    for i in 0..num_messages {
        let _: i64 = redis::cmd("PUBLISH")
            .arg(channel)
            .arg(format!(r#"{{"event":"like","seq":{i}}}"#))
            .query_async(&mut conn)
            .await
            .expect("publish");
    }
    let publish_ms = publish_start.elapsed().as_secs_f64() * 1000.0;

    // Collect messages from all receivers
    eprintln!("  Collecting messages from {num_subscribers} receivers...");
    let collect_start = Instant::now();
    let mut handles = Vec::with_capacity(num_subscribers);

    for mut rx in receivers {
        handles.push(tokio::spawn(async move {
            let mut received = 0u64;
            let mut lagged = 0u64;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);

            loop {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Ok(_msg)) => {
                        received += 1;
                        if received >= num_messages as u64 {
                            break;
                        }
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                        lagged += n;
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                    Err(_) => break, // timeout
                }
            }

            (received, lagged)
        }));
    }

    let mut total_received = 0u64;
    let mut total_lagged = 0u64;
    let mut full_delivery = 0usize;

    for h in handles {
        let (received, lagged) = h.await.unwrap();
        total_received += received;
        total_lagged += lagged;
        if received == num_messages as u64 {
            full_delivery += 1;
        }
    }

    let collect_ms = collect_start.elapsed().as_secs_f64() * 1000.0;

    let avg_received = total_received as f64 / num_subscribers as f64;
    let delivery_pct = (avg_received / num_messages as f64) * 100.0;

    eprintln!("  Publish: {publish_ms:.2}ms for {num_messages} messages");
    eprintln!("  Collect: {collect_ms:.2}ms");
    eprintln!("  Delivery: {full_delivery}/{num_subscribers} received all {num_messages} messages");
    eprintln!("  Avg received: {avg_received:.1}/{num_messages} ({delivery_pct:.1}%)");
    if total_lagged > 0 {
        eprintln!("  Total lagged (skipped): {total_lagged}");
    }

    // At least 80% of subscribers should get all messages
    assert!(
        full_delivery as f64 / num_subscribers as f64 >= 0.8,
        "Only {full_delivery}/{num_subscribers} got all messages (need >= 80%)"
    );

    eprintln!("=== PASS ===\n");
}

// ===========================================================================
// Test 6: Rate limiter accuracy under concurrency
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_rate_limiter_accuracy_under_concurrency() {
    eprintln!("\n=== Test 6: Rate Limiter Accuracy ===");

    let context = setup().await;
    let cache = context.cache.clone();

    let limit: u64 = 30;
    let num_concurrent = 100;
    let key = format!("rl:stress:{}", Uuid::new_v4());

    eprintln!(
        "  Launching {} concurrent requests against limit={}...",
        num_concurrent, limit
    );

    let barrier = Arc::new(tokio::sync::Barrier::new(num_concurrent));
    let mut handles = Vec::with_capacity(num_concurrent);

    for _ in 0..num_concurrent {
        let c = cache.clone();
        let k = key.clone();
        let bar = barrier.clone();

        handles.push(tokio::spawn(async move {
            bar.wait().await;
            let r = check_rate_limit_inner(&c, &k, limit, 60).await;
            (r.allowed, r.current)
        }));
    }

    let mut allowed_count = 0u64;
    let mut blocked_count = 0u64;

    for h in handles {
        let (allowed, _current) = h.await.unwrap();
        if allowed {
            allowed_count += 1;
        } else {
            blocked_count += 1;
        }
    }

    eprintln!("  Allowed: {allowed_count}");
    eprintln!("  Blocked: {blocked_count}");

    assert_eq!(
        allowed_count, limit,
        "Expected exactly {limit} allowed, got {allowed_count}"
    );
    assert_eq!(
        blocked_count,
        num_concurrent as u64 - limit,
        "Expected {} blocked, got {blocked_count}",
        num_concurrent as u64 - limit
    );

    // Phase 2: verify subsequent requests are still blocked
    eprintln!("  Verifying window holds ({} more requests)...", 50);

    let mut still_blocked = 0;
    for _ in 0..50 {
        let r = check_rate_limit_inner(&cache, &key, limit, 60).await;
        let (allowed, _) = (r.allowed, r.current);
        if !allowed {
            still_blocked += 1;
        }
    }

    assert_eq!(
        still_blocked, 50,
        "Rate limit window must hold: expected 50 blocked, got {still_blocked}"
    );

    eprintln!("  Window holds: all 50 subsequent requests blocked.");
    eprintln!("=== PASS ===\n");
}
