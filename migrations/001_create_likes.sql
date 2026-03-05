-- Create likes table
CREATE TABLE IF NOT EXISTS likes (
    id           BIGSERIAL PRIMARY KEY,
    user_id      UUID NOT NULL,
    content_type VARCHAR(50) NOT NULL,
    content_id   UUID NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT uq_likes_user_content UNIQUE (user_id, content_type, content_id)
);

-- User's liked items: cursor pagination ordered by (created_at DESC, id DESC)
-- Covers: GET /v1/likes/user?content_type=X&cursor=Y&limit=Z
CREATE INDEX IF NOT EXISTS idx_likes_user_created
    ON likes (user_id, created_at DESC, id DESC);

-- Count aggregation fallback when cache misses
-- Covers: SELECT COUNT(*) FROM likes WHERE content_type = $1 AND content_id = $2
CREATE INDEX IF NOT EXISTS idx_likes_content
    ON likes (content_type, content_id);

-- Time-windowed leaderboard aggregation
-- Covers: GROUP BY (content_type, content_id) WHERE created_at > NOW() - interval
CREATE INDEX IF NOT EXISTS idx_likes_created_at
    ON likes (created_at);
