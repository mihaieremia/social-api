use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// Like record from the database.
#[derive(Debug, sqlx::FromRow)]
pub struct LikeRow {
    pub id: i64,
    #[allow(dead_code)]
    pub user_id: Uuid,
    pub content_type: String,
    pub content_id: Uuid,
    pub created_at: DateTime<Utc>,
}

/// Insert a like. Returns the row if inserted, None if already existed (idempotent).
/// Atomically increments like_counts in a single transaction.
pub async fn insert_like(
    pool: &PgPool,
    user_id: Uuid,
    content_type: &str,
    content_id: Uuid,
) -> Result<(LikeRow, bool), sqlx::Error> {
    let mut tx = pool.begin().await?;

    // INSERT ON CONFLICT DO NOTHING - idempotent
    let row = sqlx::query_as::<_, LikeRow>(
        r#"
        INSERT INTO likes (user_id, content_type, content_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (user_id, content_type, content_id) DO NOTHING
        RETURNING id, user_id, content_type, content_id, created_at
        "#,
    )
    .bind(user_id)
    .bind(content_type)
    .bind(content_id)
    .fetch_optional(&mut *tx)
    .await?;

    match row {
        Some(like_row) => {
            // New like - increment count atomically
            sqlx::query(
                r#"
                INSERT INTO like_counts (content_type, content_id, total_count, updated_at)
                VALUES ($1, $2, 1, NOW())
                ON CONFLICT (content_type, content_id)
                DO UPDATE SET total_count = like_counts.total_count + 1, updated_at = NOW()
                "#,
            )
            .bind(content_type)
            .bind(content_id)
            .execute(&mut *tx)
            .await?;

            tx.commit().await?;
            Ok((like_row, false)) // (row, already_existed=false)
        }
        None => {
            // Already existed - fetch the existing row
            tx.commit().await?;
            let existing = sqlx::query_as::<_, LikeRow>(
                r#"
                SELECT id, user_id, content_type, content_id, created_at
                FROM likes
                WHERE user_id = $1 AND content_type = $2 AND content_id = $3
                "#,
            )
            .bind(user_id)
            .bind(content_type)
            .bind(content_id)
            .fetch_one(pool)
            .await?;

            Ok((existing, true)) // (row, already_existed=true)
        }
    }
}

/// Delete a like. Returns true if a row was deleted.
/// Atomically decrements like_counts in a single transaction.
pub async fn delete_like(
    pool: &PgPool,
    user_id: Uuid,
    content_type: &str,
    content_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let result = sqlx::query(
        r#"
        DELETE FROM likes
        WHERE user_id = $1 AND content_type = $2 AND content_id = $3
        "#,
    )
    .bind(user_id)
    .bind(content_type)
    .bind(content_id)
    .execute(&mut *tx)
    .await?;

    if result.rows_affected() > 0 {
        // Decrement count atomically
        sqlx::query(
            r#"
            UPDATE like_counts
            SET total_count = GREATEST(total_count - 1, 0), updated_at = NOW()
            WHERE content_type = $1 AND content_id = $2
            "#,
        )
        .bind(content_type)
        .bind(content_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(true)
    } else {
        tx.commit().await?;
        Ok(false)
    }
}

/// Get the like count for a content item from the like_counts table.
pub async fn get_count(
    pool: &PgPool,
    content_type: &str,
    content_id: Uuid,
) -> Result<i64, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        r#"
        SELECT total_count FROM like_counts
        WHERE content_type = $1 AND content_id = $2
        "#,
    )
    .bind(content_type)
    .bind(content_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(c,)| c).unwrap_or(0))
}

/// Check if a user has liked a content item.
pub async fn get_like_status(
    pool: &PgPool,
    user_id: Uuid,
    content_type: &str,
    content_id: Uuid,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    let row: Option<(DateTime<Utc>,)> = sqlx::query_as(
        r#"
        SELECT created_at FROM likes
        WHERE user_id = $1 AND content_type = $2 AND content_id = $3
        "#,
    )
    .bind(user_id)
    .bind(content_type)
    .bind(content_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(ts,)| ts))
}

/// Get a user's liked items with cursor-based pagination.
/// Returns limit+1 rows to detect has_more.
pub async fn get_user_likes(
    pool: &PgPool,
    user_id: Uuid,
    content_type_filter: Option<&str>,
    cursor_ts: Option<DateTime<Utc>>,
    cursor_id: Option<i64>,
    limit: i64,
) -> Result<Vec<LikeRow>, sqlx::Error> {
    let fetch_limit = limit + 1;

    match (cursor_ts, cursor_id, content_type_filter) {
        (Some(ts), Some(id), Some(ct)) => {
            sqlx::query_as::<_, LikeRow>(
                r#"
                SELECT id, user_id, content_type, content_id, created_at
                FROM likes
                WHERE user_id = $1 AND content_type = $2
                  AND (created_at, id) < ($3, $4)
                ORDER BY created_at DESC, id DESC
                LIMIT $5
                "#,
            )
            .bind(user_id)
            .bind(ct)
            .bind(ts)
            .bind(id)
            .bind(fetch_limit)
            .fetch_all(pool)
            .await
        }
        (Some(ts), Some(id), None) => {
            sqlx::query_as::<_, LikeRow>(
                r#"
                SELECT id, user_id, content_type, content_id, created_at
                FROM likes
                WHERE user_id = $1
                  AND (created_at, id) < ($2, $3)
                ORDER BY created_at DESC, id DESC
                LIMIT $4
                "#,
            )
            .bind(user_id)
            .bind(ts)
            .bind(id)
            .bind(fetch_limit)
            .fetch_all(pool)
            .await
        }
        (_, _, Some(ct)) => {
            sqlx::query_as::<_, LikeRow>(
                r#"
                SELECT id, user_id, content_type, content_id, created_at
                FROM likes
                WHERE user_id = $1 AND content_type = $2
                ORDER BY created_at DESC, id DESC
                LIMIT $3
                "#,
            )
            .bind(user_id)
            .bind(ct)
            .bind(fetch_limit)
            .fetch_all(pool)
            .await
        }
        _ => {
            sqlx::query_as::<_, LikeRow>(
                r#"
                SELECT id, user_id, content_type, content_id, created_at
                FROM likes
                WHERE user_id = $1
                ORDER BY created_at DESC, id DESC
                LIMIT $2
                "#,
            )
            .bind(user_id)
            .bind(fetch_limit)
            .fetch_all(pool)
            .await
        }
    }
}

/// Batch get like counts from the like_counts table.
pub async fn batch_get_counts(
    pool: &PgPool,
    items: &[(String, Uuid)],
) -> Result<Vec<(String, Uuid, i64)>, sqlx::Error> {
    if items.is_empty() {
        return Ok(vec![]);
    }

    // Build dynamic query for batch lookup
    let mut content_types = Vec::with_capacity(items.len());
    let mut content_ids = Vec::with_capacity(items.len());
    for (ct, cid) in items {
        content_types.push(ct.as_str());
        content_ids.push(*cid);
    }

    // Use unnest for efficient batch lookup
    let rows: Vec<(String, Uuid, i64)> = sqlx::query_as(
        r#"
        SELECT lc.content_type, lc.content_id, lc.total_count
        FROM unnest($1::text[], $2::uuid[]) AS req(content_type, content_id)
        LEFT JOIN like_counts lc ON lc.content_type = req.content_type AND lc.content_id = req.content_id
        "#,
    )
    .bind(&content_types)
    .bind(&content_ids)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Batch get like statuses for a user.
pub async fn batch_get_statuses(
    pool: &PgPool,
    user_id: Uuid,
    items: &[(String, Uuid)],
) -> Result<Vec<(String, Uuid, Option<DateTime<Utc>>)>, sqlx::Error> {
    if items.is_empty() {
        return Ok(vec![]);
    }

    let mut content_types = Vec::with_capacity(items.len());
    let mut content_ids = Vec::with_capacity(items.len());
    for (ct, cid) in items {
        content_types.push(ct.as_str());
        content_ids.push(*cid);
    }

    let rows: Vec<(String, Uuid, Option<DateTime<Utc>>)> = sqlx::query_as(
        r#"
        SELECT req.content_type, req.content_id, l.created_at
        FROM unnest($1::text[], $2::uuid[]) AS req(content_type, content_id)
        LEFT JOIN likes l ON l.user_id = $3 AND l.content_type = req.content_type AND l.content_id = req.content_id
        "#,
    )
    .bind(&content_types)
    .bind(&content_ids)
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Get top liked content within a time window.
pub async fn get_leaderboard(
    pool: &PgPool,
    content_type: Option<&str>,
    since: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<Vec<(String, Uuid, i64)>, sqlx::Error> {
    match (content_type, since) {
        (Some(ct), Some(since_ts)) => {
            sqlx::query_as(
                r#"
                SELECT content_type, content_id, COUNT(*) as count
                FROM likes
                WHERE content_type = $1 AND created_at >= $2
                GROUP BY content_type, content_id
                ORDER BY count DESC
                LIMIT $3
                "#,
            )
            .bind(ct)
            .bind(since_ts)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (Some(ct), None) => {
            sqlx::query_as(
                r#"
                SELECT content_type, content_id, total_count as count
                FROM like_counts
                WHERE content_type = $1
                ORDER BY total_count DESC
                LIMIT $2
                "#,
            )
            .bind(ct)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (None, Some(since_ts)) => {
            sqlx::query_as(
                r#"
                SELECT content_type, content_id, COUNT(*) as count
                FROM likes
                WHERE created_at >= $1
                GROUP BY content_type, content_id
                ORDER BY count DESC
                LIMIT $2
                "#,
            )
            .bind(since_ts)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (None, None) => {
            sqlx::query_as(
                r#"
                SELECT content_type, content_id, total_count as count
                FROM like_counts
                ORDER BY total_count DESC
                LIMIT $1
                "#,
            )
            .bind(limit)
            .fetch_all(pool)
            .await
        }
    }
}
