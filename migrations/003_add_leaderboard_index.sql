-- Migration: 003_add_leaderboard_index.sql
-- Adds a composite index to speed up time-windowed leaderboard queries
-- with content_type filter.
--
-- Query this covers:
--   SELECT content_type, content_id, COUNT(*) as count
--   FROM likes
--   WHERE content_type = $1 AND created_at >= $2
--   GROUP BY content_type, content_id ORDER BY count DESC LIMIT $3
--
-- Without this index: sequential scan or idx_likes_content scan (no time filter)
-- With this index: index range scan on (content_type, created_at) — O(matching rows)
--
-- Write overhead: ~1 insert/second average at expected load — negligible.
CREATE INDEX IF NOT EXISTS idx_likes_content_type_created_at
    ON likes (content_type, created_at DESC);
