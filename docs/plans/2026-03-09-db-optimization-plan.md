# Database & Leaderboard Optimization — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix leaderboard refresh latency spikes, drop redundant indexes, precompute filtered leaderboards, and reorder the leaderboard index for filtered queries.

**Architecture:** Sequential refresh eliminates reader pool contention. Precomputed filtered leaderboards remove on-demand DB queries. Index reordering improves filtered query I/O by 3-5x.

**Tech Stack:** Rust, SQLx migrations, Axum, Redis (bb8), moka L1, tokio

**Design doc:** `docs/plans/2026-03-09-db-optimization-design.md`

---

## Task 1: Sequential Leaderboard Refresh

**Files:**
- Modify: `crates/social-api/src/tasks/leaderboard_refresh.rs`

**Step 1: Replace parallel `join_all` with sequential loop**

In `refresh_all_windows`, replace lines 78-89:

```rust
// OLD (parallel):
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

// NEW (sequential):
for &window in &WINDOWS {
    if let Err(e) = refresh_window(db, cache, window).await {
        tracing::warn!(error = %e, "Failed to refresh leaderboard window");
    }
}
```

Also remove `use futures::future::join_all;` from the top if it becomes unused.

**Step 2: Run existing tests**

```bash
cargo test --package social-api leaderboard_refresh -- --nocapture
```

Expected: All 5 existing tests pass (no behavioral change, just execution order).

**Step 3: Remove `futures` dep if unused**

Check if `futures::future::join_all` was the only use of the `futures` crate in this file. If so, remove the import. If `futures` is still used elsewhere in the crate, leave the workspace dep.

```bash
grep -r "futures" crates/social-api/src/ --include="*.rs" | grep -v "test" | grep -v "target"
```

**Step 4: Commit**

```
feat(refresh): run leaderboard windows sequentially to avoid reader pool contention
```

---

## Task 2: Precompute Filtered Leaderboards in Refresh Task

**Files:**
- Modify: `crates/social-api/src/tasks/leaderboard_refresh.rs` (add `Config` param, per-type loop)
- Modify: `crates/social-api/src/main.rs` (pass `config` to `spawn_leaderboard_refresh`)
- Modify: `crates/social-api/src/services/like_queries.rs` (remove DB fallback for filtered leaderboard)

**Step 1: Thread `Config` into refresh functions**

`spawn_leaderboard_refresh` already receives `config: Config`. Thread it into `refresh_all_windows` and `refresh_window`:

```rust
async fn refresh_all_windows(
    db: &DbPools,
    cache: &CacheManager,
    config: &Config,  // <-- add
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for &window in &WINDOWS {
        if let Err(e) = refresh_window(db, cache, config, window).await {
            tracing::warn!(error = %e, "Failed to refresh leaderboard window");
        }
    }
    cache.invalidate_l1_prefix("lbf:");
    Ok(())
}

async fn refresh_window(
    db: &DbPools,
    cache: &CacheManager,
    config: &Config,  // <-- add
    window: TimeWindow,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // ... existing unfiltered logic stays unchanged ...

    // NEW: precompute filtered leaderboards per content type
    let since = window
        .duration_secs()
        .map(|secs| Utc::now() - chrono::Duration::seconds(secs));

    for content_type in config.content_api_urls.keys() {
        let filtered_rows =
            repositories::get_leaderboard(&db.reader, Some(content_type.as_str()), since, LEADERBOARD_LIMIT)
                .await;

        match filtered_rows {
            Ok(rows) => {
                let items: Vec<shared::types::TopLikedItem> = rows
                    .into_iter()
                    .map(|(ct, cid, count)| shared::types::TopLikedItem {
                        content_type: ct,
                        content_id: cid,
                        count,
                    })
                    .collect();

                if let Ok(json) = serde_json::to_string(&items) {
                    let key = format!(
                        "lbf:{}:{}:{}",
                        window.as_str(),
                        content_type,
                        LEADERBOARD_LIMIT
                    );
                    cache.set(&key, &json, config.cache_ttl_leaderboard_filtered_secs).await;
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    window = window.as_str(),
                    content_type = content_type.as_str(),
                    "Failed to refresh filtered leaderboard"
                );
            }
        }
    }

    Ok(())
}
```

Update calls in the task loop and initial refresh:

```rust
// In spawn_leaderboard_refresh:
if let Err(e) = refresh_all_windows(&db, &cache, &config).await {
    // ...
}
// ... and in the loop body:
if let Err(e) = refresh_all_windows(&db, &cache, &config).await {
    // ...
}
```

Update public test wrappers:

```rust
pub async fn refresh_all_windows_public(
    db: &DbPools,
    cache: &CacheManager,
    config: &Config,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    refresh_all_windows(db, cache, config).await
}

pub async fn refresh_window_public(
    db: &DbPools,
    cache: &CacheManager,
    config: &Config,
    window: TimeWindow,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    refresh_window(db, cache, config, window).await
}
```

**Step 2: Update tests in `leaderboard_refresh.rs`**

All test calls to `refresh_all_windows` and `refresh_window` need `&config`:

```rust
// In setup(), add config to TestHarness:
struct TestHarness {
    _scope: crate::test_containers::TestScope,
    db: DbPools,
    cache: CacheManager,
    config: crate::config::Config,  // <-- add
}

// In setup():
let config = crate::config::Config::new_for_test();
// ... existing code ...
TestHarness { _scope: scope, db, cache, config }

// In tests, pass &harness.config:
refresh_all_windows(&db, &cache, &harness.config).await;
refresh_window(&db, &cache, &harness.config, TimeWindow::All).await;
// etc.
```

Also update `spawn_leaderboard_refresh` test — already passes config, no change needed.

**Step 3: Simplify filtered leaderboard read path**

In `like_queries.rs`, the `get_leaderboard` method has a DB fallback for filtered queries (lines 348-381). Since the refresh task now precomputes these, simplify the cache-miss path to return empty instead of querying DB:

```rust
// In get_leaderboard(), replace the filtered cache-miss block (lines 348-381):
} else if let Some(ct) = content_type {
    let cache_key = format!("lbf:{}:{ct}:{limit}", window.as_str());

    if let Some(cached) = self.cache.get(&cache_key).await
        && let Ok(items) = serde_json::from_str::<Vec<TopLikedItem>>(&cached)
    {
        return Ok(TopLikedResponse {
            window: window.as_str().to_string(),
            content_type: Some(ct.to_string()),
            items,
        });
    }

    // Cache miss: refresh task populates this every 60s.
    // Return empty rather than hitting DB on-demand.
    tracing::debug!(
        window = window.as_str(),
        content_type = ct,
        "Filtered leaderboard cache miss — awaiting next refresh cycle"
    );
    return Ok(TopLikedResponse {
        window: window.as_str().to_string(),
        content_type: Some(ct.to_string()),
        items: vec![],
    });
}
```

**Step 4: Run all tests**

```bash
cargo test --package social-api -- --nocapture
```

Expected: All tests pass. The `test_get_leaderboard_with_content_type_filter` test in `like_service.rs` does a like then queries the filtered leaderboard — this will now return empty from cache (no refresh task in test). If this test fails, update it to populate the cache via `refresh_window_public` first.

**Step 5: Commit**

```
feat(refresh): precompute filtered leaderboards per content type in refresh task
```

---

## Task 3: Drop Redundant Indexes

**Files:**
- Create: `migrations/003_drop_redundant_indexes.sql`

**Step 1: Create migration**

```sql
-- Drop unused indexes to reduce write amplification.
--
-- idx_likes_content: "fallback for count aggregation" but like_counts table
-- handles all count lookups. No query path references this index.
--
-- idx_likes_content_type_created_at: created in migration 002 for filtered
-- leaderboard queries, but the actual query uses idx_likes_created_ct_cid
-- (migration 001). This index is dead weight.
DROP INDEX IF EXISTS idx_likes_content;
DROP INDEX IF EXISTS idx_likes_content_type_created_at;
```

**Step 2: Run migration locally**

```bash
docker compose up -d postgres && sleep 2
cargo test --package social-api test_refresh_all_windows_empty_db -- --nocapture
```

Expected: Migration runs via embedded `sqlx::migrate!()`. Tests pass.

**Step 3: Verify indexes dropped**

```bash
docker compose exec postgres psql -U social -d social_api -c "\di likes*"
```

Expected: `idx_likes_content` and `idx_likes_content_type_created_at` are gone. Remaining: `likes_pkey`, `uq_likes_user_content`, `idx_likes_user_created`, `idx_likes_created_ct_cid`.

**Step 4: Run full test suite**

```bash
cargo test --package social-api -- --nocapture
```

Expected: All tests pass (no query depended on dropped indexes).

**Step 5: Commit**

```
refactor(db): drop unused idx_likes_content and idx_likes_content_type_created_at indexes
```

---

## Task 4: Reorder Leaderboard Index

**Files:**
- Create: `migrations/004_reorder_leaderboard_index.sql`

**Step 1: Create migration**

```sql
-- Reorder leaderboard index for filtered query performance.
--
-- Old: (created_at, content_type, content_id) — unfiltered scans work,
-- but filtered queries (WHERE content_type = $1 AND created_at >= $2)
-- must scan all date-range rows then post-filter by type.
--
-- New: (content_type, created_at DESC, content_id) — filtered queries
-- seek directly to the content_type partition, then range-scan created_at.
-- Unfiltered queries still work via merge of per-type ranges.
DROP INDEX IF EXISTS idx_likes_created_ct_cid;

CREATE INDEX idx_likes_ct_created_cid
    ON likes (content_type, created_at DESC, content_id);
```

**Step 2: Run tests**

```bash
cargo test --package social-api -- --nocapture
```

Expected: All tests pass. Leaderboard refresh tests still work (query is the same, just the index layout changed).

**Step 3: Verify new index exists**

```bash
docker compose exec postgres psql -U social -d social_api -c "\di likes*"
```

Expected: `idx_likes_ct_created_cid` present. `idx_likes_created_ct_cid` gone.

**Step 4: Verify query plan uses new index**

```bash
docker compose exec postgres psql -U social -d social_api -c "
EXPLAIN (ANALYZE, BUFFERS)
SELECT content_type, content_id, COUNT(*) as count
FROM likes
WHERE content_type = 'post' AND created_at >= NOW() - INTERVAL '24 hours'
GROUP BY content_type, content_id
ORDER BY count DESC
LIMIT 50;
"
```

Expected: Index Scan or Index Only Scan on `idx_likes_ct_created_cid`.

**Step 5: Commit**

```
perf(db): reorder leaderboard index to (content_type, created_at DESC, content_id)
```

---

## Task 5: Fix Test for Filtered Leaderboard (if needed)

**Files:**
- Modify: `crates/social-api/src/services/like_service.rs` (test block only)

**Step 1: Check if `test_get_leaderboard_with_content_type_filter` passes**

After Task 2, filtered leaderboard returns empty on cache miss (no refresh task in tests). If this test fails:

```rust
#[tokio::test]
async fn test_get_leaderboard_with_content_type_filter() {
    let harness = make_service(Arc::new(AlwaysValidContent)).await;
    let svc = harness.svc;
    let user_id = Uuid::new_v4();
    let post_id = Uuid::new_v4();

    svc.like(user_id, "lb_svc_test", post_id).await.unwrap();

    // Populate filtered leaderboard cache (refresh task does this in prod)
    let db = /* get db from harness */;
    crate::tasks::leaderboard_refresh::refresh_window_public(
        &db, &harness.cache, &config, TimeWindow::All
    ).await.unwrap();

    let resp = svc
        .get_leaderboard(Some("lb_svc_test"), TimeWindow::All, 10)
        .await
        .unwrap();

    assert_eq!(resp.window, "all");
    assert_eq!(resp.content_type, Some("lb_svc_test".to_string()));
    assert!(!resp.items.is_empty(), "expected at least one item");
    assert_eq!(resp.items[0].count, 1);
}
```

If the test already passes (because `lb_svc_test` is not in `config.content_api_urls` so it falls through to the unfiltered ZSET path), no change needed.

**Step 2: Run full suite**

```bash
cargo test --package social-api -- --nocapture
```

**Step 3: Commit (only if changes were needed)**

```
fix(test): populate filtered leaderboard cache in service test
```

---

## Task 6: Rebuild and Load Test

**Step 1: Rebuild Docker image**

```bash
make down && make build && make up
```

**Step 2: Run k6 load test**

```bash
make k6-load
```

**Step 3: Check Grafana**

- Verify: No more latency spikes correlated with leaderboard refresh
- Verify: p90/p99 latency is flat during refresh cycles
- Verify: Filtered leaderboard requests have 0 cache-miss DB queries

**Step 4: Commit dashboard if updated**

```
test(k6): verify Phase 1 optimizations under load
```
