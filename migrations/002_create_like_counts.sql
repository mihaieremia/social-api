-- Materialized counter table for O(1) like count reads.
-- Avoids COUNT(*) on every read request.
-- Write cost: one extra UPDATE per like/unlike (acceptable for 80/20 read/write ratio).
CREATE TABLE IF NOT EXISTS like_counts (
    content_type VARCHAR(50) NOT NULL,
    content_id   UUID NOT NULL,
    total_count  BIGINT NOT NULL DEFAULT 0,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (content_type, content_id),
    CONSTRAINT chk_total_count_non_negative CHECK (total_count >= 0)
);
