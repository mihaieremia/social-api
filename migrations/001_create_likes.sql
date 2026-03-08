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

-- Time-windowed leaderboard aggregation with content_type filter support.
-- Filtered queries seek to content_type partition, then range-scan created_at.
-- Unfiltered queries still work via merge of per-type ranges.
CREATE INDEX IF NOT EXISTS idx_likes_ct_created_cid
    ON likes (content_type, created_at DESC, content_id);
