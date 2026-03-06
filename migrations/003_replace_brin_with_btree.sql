-- Replace BRIN index on created_at with a B-tree composite index.
--
-- The leaderboard query is:
--   SELECT content_type, content_id, COUNT(*)
--   FROM likes WHERE created_at >= $cutoff
--   GROUP BY content_type, content_id ORDER BY count DESC LIMIT 50
--
-- BRIN only eliminates page ranges — it still requires a full scan of matching
-- pages followed by hash aggregation.  A B-tree on (created_at, content_type,
-- content_id) lets Postgres do an index range scan feeding directly into the
-- GROUP BY, which is significantly faster for short sliding windows (24h, 7d).

DROP INDEX IF EXISTS idx_likes_created_at;

CREATE INDEX IF NOT EXISTS idx_likes_created_ct_cid
    ON likes (created_at, content_type, content_id);
