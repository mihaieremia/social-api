# Extended Caching — Like Status, User Likes, Filtered Leaderboard

## Problem

Four read paths hit the database on every request with no caching:
1. **Like status** — single user-content check
2. **Batch statuses** — up to 100 user-content checks in one call
3. **User likes** — paginated feed of a user's liked items
4. **Filtered leaderboard** — leaderboard with content_type filter

At 10k+ RPS, these create unnecessary DB pressure when L1+Redis could absorb most reads.

## Solution

### 1. Like Status Cache

| Attribute | Value |
|-----------|-------|
| Key | `ls:{user_id}:{content_type}:{content_id}` |
| Value | ISO timestamp (liked) or `""` (not liked) |
| TTL | `CACHE_TTL_LIKE_STATUS_SECS` (default 300s) |

- **Read**: L1 → Redis → DB. Cache result on miss.
- **Write-through**: On like → SET with timestamp. On unlike → SET with `""`.
- Empty string distinguishes "checked, not liked" from cache miss (unknown).

### 2. Batch Statuses (uses same `ls:` keys)

- Build per-item `ls:` keys → `cache.mget()` (L1 → Redis)
- DB batch query for misses only → `cache.set_many()` results
- Reduces DB pressure from O(batch_size) to O(cache_misses)

### 3. User Likes (first page only)

| Attribute | Value |
|-----------|-------|
| Key | `ul:{user_id}:{content_type_filter\|_}` |
| Value | JSON serialized `CachedUserLikePage { rows, has_more }` |
| TTL | `CACHE_TTL_USER_LIKES_SECS` (default 60s) |

- Only cache when `cursor=None` (first page). Subsequent pages bypass cache.
- Always store full first page (up to 101 rows at max limit) and slice to requested limit.
- **Invalidation**: On like/unlike:
  - L1: `invalidate_entries_if` with prefix `ul:{user_id}:`
  - Redis: DEL `ul:{user_id}:_` + DEL `ul:{user_id}:{content_type}` (2 explicit DELs)

### 4. Filtered Leaderboard

| Attribute | Value |
|-----------|-------|
| Key | `lbf:{window}:{content_type}:{limit}` |
| Value | JSON serialized `Vec<TopLikedItem>` |
| TTL | `CACHE_TTL_LEADERBOARD_FILTERED_SECS` (default 60s) |

- Only when content_type filter is present. Unfiltered uses existing ZSET path.
- **Invalidation**: On leaderboard refresh task:
  - L1: `invalidate_entries_if` with prefix `lbf:`
  - Redis: TTL expiry (60s aligned with refresh interval)

## Invalidation Matrix

| Event | Like Status | User Likes | Filtered LB | Count |
|-------|-------------|------------|-------------|-------|
| like | SET ts | DEL prefix | — | write_count (existing) |
| unlike | SET "" | DEL prefix | — | write_count (existing) |
| LB refresh | — | — | L1 prefix invalidate | — |

## New Config

```env
CACHE_TTL_LIKE_STATUS_SECS=300
CACHE_TTL_USER_LIKES_SECS=60
CACHE_TTL_LEADERBOARD_FILTERED_SECS=60
```

## New Redis Keys

| Key Pattern | Type | TTL | Purpose |
|-------------|------|-----|---------|
| `ls:{user_id}:{type}:{id}` | STRING | 300s | Per-user like status |
| `ul:{user_id}:{type\|_}` | STRING | 60s | User likes first page (JSON) |
| `lbf:{window}:{type}:{limit}` | STRING | 60s | Filtered leaderboard (JSON) |
