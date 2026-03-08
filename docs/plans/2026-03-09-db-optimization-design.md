# Database & Leaderboard Optimization Design

**Date:** 2026-03-09
**Status:** Approved
**Scope:** Sequential refresh, index cleanup, filtered precomputation, windowed counters

## Problem

The leaderboard refresh task runs four heavy `GROUP BY COUNT(*)` queries in parallel every 60 seconds. These queries compete with API read traffic for the reader pool (20 connections), causing 2-3.6s latency spikes on all read endpoints.

At production scale (100M+ likes, millions of content items), the time-windowed `GROUP BY` scan becomes untenable regardless of concurrency.

## Data Model Context

- **`likes`**: Write-heavy, append-mostly. Grows linearly with traffic.
- **`like_counts`**: Materialized all-time counter. Small (one row per content item).
- **Like distribution**: Long-tail — initial burst on new content, then slow trickle.
- **Content types**: 3-5 known types, loaded from config (`CONTENT_API_{TYPE}_URL`).

## Phase 1: Fix Current Latency Spikes

### 1.1 Sequential Leaderboard Refresh

Replace `join_all` with a sequential loop in `refresh_all_windows`.

**File:** `crates/social-api/src/tasks/leaderboard_refresh.rs`

```rust
// Before (parallel — saturates reader pool):
let futures: Vec<_> = WINDOWS.iter()
    .map(|&window| refresh_window(db, cache, window))
    .collect();
join_all(futures).await;

// After (sequential — one heavy query at a time):
for &window in &WINDOWS {
    if let Err(e) = refresh_window(db, cache, &config, window).await {
        tracing::warn!(error = %e, "Failed to refresh leaderboard window");
    }
}
```

Total refresh takes ~4x longer wall-clock, but never holds more than 1 reader pool connection for heavy work. Background task — latency does not matter.

### 1.2 Precompute Filtered Leaderboards

Extend `refresh_window` to also compute per-type leaderboards and cache them. Eliminates all on-demand DB queries for filtered leaderboards.

```rust
async fn refresh_window(db, cache, config, window) {
    // 1. Unfiltered query (existing)
    let rows = repositories::get_leaderboard(&db.reader, None, since, LIMIT).await?;
    cache.replace_sorted_set(&format!("lb:{window}"), &members).await;

    // 2. Per-type filtered queries (new)
    for content_type in config.content_api_urls.keys() {
        let filtered = repositories::get_leaderboard(
            &db.reader, Some(content_type), since, LIMIT
        ).await?;
        let items: Vec<TopLikedItem> = /* convert */;
        let key = format!("lbf:{}:{}:{}", window, content_type, LIMIT);
        cache.set(&key, &serde_json::to_string(&items)?, ttl).await;
    }
}
```

With 3-5 content types × 4 windows = 12-20 queries, all sequential. Each query is fast (uses index). Total refresh cycle: ~5-10 seconds.

**Read path change:** `LikeQueryService::get_leaderboard` skips the DB fallback for filtered queries — if cache misses, return empty or stale data. The refresh task guarantees population within 60s.

### 1.3 Drop Redundant Indexes

**Migration:** `005_drop_redundant_indexes.sql`

```sql
-- Never used: like_counts handles count lookups, not likes table scans
DROP INDEX IF EXISTS idx_likes_content;

-- Dead index from migration 002: no query path references it
DROP INDEX IF EXISTS idx_likes_content_type_created_at;
```

**Impact:** Reduces write amplification on every INSERT/DELETE. At 100M rows, saves ~2-4 GB of index storage.

### 1.4 Reorder Leaderboard Index

**Migration:** `006_reorder_leaderboard_index.sql`

```sql
DROP INDEX IF EXISTS idx_likes_created_ct_cid;

-- Filtered queries seek to content_type first, then range scan created_at.
-- Unfiltered queries still work via merge of per-type ranges.
CREATE INDEX idx_likes_ct_created_cid
    ON likes (content_type, created_at DESC, content_id);
```

**Before (unfiltered 7d window, 100M rows):** Scans all rows in date range, groups by type.
**After:** Same cost for unfiltered. ~3-5x faster for filtered (seeks to single type partition).

## Phase 2: Eliminate GROUP BY Scans at Scale

When `likes` exceeds ~50M rows, even sequential GROUP BY queries take 10-30+ seconds. The windowed counter table eliminates these scans.

### 2.1 Windowed Counter Table

**Migration:** `007_create_like_counts_windowed.sql`

```sql
CREATE TABLE like_counts_windowed (
    content_type  VARCHAR(50) NOT NULL,
    content_id    UUID NOT NULL,
    window        VARCHAR(10) NOT NULL,  -- '24h', '7d', '30d'
    like_count    BIGINT NOT NULL DEFAULT 0,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (content_type, content_id, window),
    CHECK (like_count >= 0)
);
```

### 2.2 Write Path Changes

On like/unlike, update windowed counters alongside `like_counts` in the same transaction.

```rust
// In repositories::insert_like, after like_counts UPDATE:
sqlx::query(r#"
    INSERT INTO like_counts_windowed (content_type, content_id, window, like_count)
    SELECT $1, $2, w.window, 1
    FROM unnest(ARRAY['24h','7d','30d']::text[]) AS w(window)
    ON CONFLICT (content_type, content_id, window)
    DO UPDATE SET like_count = like_counts_windowed.like_count + 1,
                  updated_at = NOW()
"#)
```

Cost: one additional multi-row UPSERT per like/unlike. Marginal overhead since it's in the same transaction and uses the PK.

### 2.3 Cleanup Task: Expire Aged-Out Likes

Windowed counters drift as likes age out of their window. A periodic cleanup task corrects them.

**Schedule:** Every 15 minutes (configurable).

```rust
async fn cleanup_windowed_counts(db: &DbPools) {
    for (window_name, duration) in [("24h", 86400), ("7d", 604800), ("30d", 2592000)] {
        let cutoff = Utc::now() - Duration::seconds(duration);

        // Count likes that aged out since last cleanup
        // and decrement windowed counters accordingly.
        sqlx::query(r#"
            WITH expired AS (
                SELECT content_type, content_id, COUNT(*) as cnt
                FROM likes
                WHERE created_at < $1
                  AND created_at >= $1 - INTERVAL '15 minutes'
                GROUP BY content_type, content_id
            )
            UPDATE like_counts_windowed w
            SET like_count = GREATEST(w.like_count - e.cnt, 0),
                updated_at = NOW()
            FROM expired e
            WHERE w.content_type = e.content_type
              AND w.content_id = e.content_id
              AND w.window = $2
        "#)
        .bind(cutoff)
        .bind(window_name)
        .execute(&db.writer)
        .await?;
    }
}
```

The cleanup query scans only 15 minutes of recently-expired rows — fast regardless of table size.

### 2.4 Leaderboard Refresh Becomes Trivial

```sql
-- Replaces GROUP BY COUNT(*) over millions of rows:
SELECT content_type, content_id, like_count
FROM like_counts_windowed
WHERE window = '24h'
ORDER BY like_count DESC
LIMIT 50
```

Uses the PK. Returns in <1ms regardless of `likes` table size.

### 2.5 Periodic Full Reconciliation

Windowed counters may drift slightly from cleanup timing. A weekly reconciliation task recalculates from source of truth:

```sql
-- Run weekly during low-traffic hours
INSERT INTO like_counts_windowed (content_type, content_id, window, like_count)
SELECT content_type, content_id, '24h', COUNT(*)
FROM likes WHERE created_at >= NOW() - INTERVAL '24 hours'
GROUP BY content_type, content_id
ON CONFLICT (content_type, content_id, window)
DO UPDATE SET like_count = EXCLUDED.like_count, updated_at = NOW();
```

This is the same expensive GROUP BY, but runs once per week — acceptable.

## Index Summary (After All Phases)

| Index | Table | Columns | Purpose |
|-------|-------|---------|---------|
| `uq_likes_user_content` | likes | `(user_id, content_type, content_id)` UNIQUE | Insert conflict, delete, status |
| `idx_likes_user_created` | likes | `(user_id, created_at DESC, id DESC)` | User likes pagination |
| `idx_likes_ct_created_cid` | likes | `(content_type, created_at DESC, content_id)` | Filtered leaderboard refresh |
| (PK) | like_counts | `(content_type, content_id)` | All-time counts |
| (PK) | like_counts_windowed | `(content_type, content_id, window)` | Windowed counts |

**Dropped:**
- `idx_likes_content` — unused
- `idx_likes_content_type_created_at` — unused
- `idx_likes_created_ct_cid` — replaced by reordered version

## Implementation Order

| Step | Change | Risk | Depends On |
|------|--------|------|------------|
| 1 | Sequential refresh loop | None | — |
| 2 | Precompute filtered leaderboards in refresh | Low | Step 1 |
| 3 | Drop redundant indexes (migration 005) | None | — |
| 4 | Reorder leaderboard index (migration 006) | Low | Step 3 |
| 5 | Create `like_counts_windowed` (migration 007) | Low | — |
| 6 | Write path: update windowed counters | Medium | Step 5 |
| 7 | Cleanup task for expired window entries | Medium | Step 6 |
| 8 | Switch refresh to use windowed counters | Low | Step 7 |
| 9 | Weekly reconciliation task | Low | Step 6 |

Steps 1-4 form Phase 1 (immediate fix). Steps 5-9 form Phase 2 (scale preparation, implement when likes > 50M).
