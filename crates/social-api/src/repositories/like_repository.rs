use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// Like record from the database.
/// Note: `user_id` is intentionally not included — callers always know the
/// user_id already (they pass it in). This avoids selecting an unused column.
#[derive(Debug, sqlx::FromRow)]
pub struct LikeRow {
    pub id: i64,
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
        RETURNING id, content_type, content_id, created_at
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
                SELECT id, content_type, content_id, created_at
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
                SELECT id, content_type, content_id, created_at
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
                SELECT id, content_type, content_id, created_at
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
                SELECT id, content_type, content_id, created_at
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
                SELECT id, content_type, content_id, created_at
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

#[cfg(test)]
mod tests {
    use super::*;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;
    use uuid::Uuid;

    /// Spin up a fresh Postgres container, run migrations, return a pool.
    /// The returned container must be kept alive for the duration of the test.
    async fn setup_pg() -> (sqlx::PgPool, testcontainers::ContainerAsync<Postgres>) {
        let pg = Postgres::default()
            .start()
            .await
            .expect("postgres container");
        let port = pg.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .expect("connect test postgres");
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        (pool, pg)
    }

    #[tokio::test]
    async fn test_insert_like_new() {
        let (pool, _pg) = setup_pg().await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let (row, existed) = insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();

        assert!(!existed);
        assert_eq!(row.content_type, "post");
        assert_eq!(row.content_id, content_id);
    }

    #[tokio::test]
    async fn test_insert_like_idempotent() {
        let (pool, _pg) = setup_pg().await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();
        let (_, existed) = insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();

        assert!(existed, "second insert must report already_existed=true");
    }

    #[tokio::test]
    async fn test_delete_like_existing() {
        let (pool, _pg) = setup_pg().await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();
        let was_liked = delete_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();

        assert!(was_liked);
    }

    #[tokio::test]
    async fn test_delete_like_not_existing() {
        let (pool, _pg) = setup_pg().await;
        let was_liked = delete_like(&pool, Uuid::new_v4(), "post", Uuid::new_v4())
            .await
            .unwrap();
        assert!(!was_liked);
    }

    #[tokio::test]
    async fn test_get_count_after_likes() {
        let (pool, _pg) = setup_pg().await;
        let content_id = Uuid::new_v4();
        for _ in 0..3 {
            insert_like(&pool, Uuid::new_v4(), "post", content_id)
                .await
                .unwrap();
        }
        let count = get_count(&pool, "post", content_id).await.unwrap();
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn test_get_count_zero_for_unknown_content() {
        let (pool, _pg) = setup_pg().await;
        let count = get_count(&pool, "post", Uuid::new_v4()).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_get_like_status_liked() {
        let (pool, _pg) = setup_pg().await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();
        let ts = get_like_status(&pool, user_id, "post", content_id)
            .await
            .unwrap();

        assert!(ts.is_some(), "liked_at must be Some after liking");
    }

    #[tokio::test]
    async fn test_get_like_status_not_liked() {
        let (pool, _pg) = setup_pg().await;
        let ts = get_like_status(&pool, Uuid::new_v4(), "post", Uuid::new_v4())
            .await
            .unwrap();
        assert!(ts.is_none());
    }

    #[tokio::test]
    async fn test_batch_get_statuses() {
        let (pool, _pg) = setup_pg().await;
        let user_id = Uuid::new_v4();
        let liked_id = Uuid::new_v4();
        let not_liked_id = Uuid::new_v4();

        insert_like(&pool, user_id, "post", liked_id).await.unwrap();

        let rows = batch_get_statuses(
            &pool,
            user_id,
            &[
                ("post".to_string(), liked_id),
                ("post".to_string(), not_liked_id),
            ],
        )
        .await
        .unwrap();

        let map: std::collections::HashMap<_, _> = rows
            .into_iter()
            .map(|(ct, cid, ts)| ((ct, cid), ts))
            .collect();

        assert!(map[&("post".to_string(), liked_id)].is_some());
        assert!(map[&("post".to_string(), not_liked_id)].is_none());
    }

    #[tokio::test]
    async fn test_get_user_likes_pagination() {
        let (pool, _pg) = setup_pg().await;
        let user_id = Uuid::new_v4();
        for _ in 0..5 {
            insert_like(&pool, user_id, "post", Uuid::new_v4())
                .await
                .unwrap();
        }

        // Fetch limit+1 to detect has_more
        let page1 = get_user_likes(&pool, user_id, None, None, None, 2)
            .await
            .unwrap();
        assert_eq!(page1.len(), 3, "must fetch limit+1 to detect has_more");

        // Use the last row as cursor
        let last = page1.last().unwrap();
        let page2 = get_user_likes(
            &pool,
            user_id,
            None,
            Some(last.created_at),
            Some(last.id),
            2,
        )
        .await
        .unwrap();

        assert!(!page2.is_empty());
        let page1_ids: std::collections::HashSet<_> = page1.iter().map(|r| r.id).collect();
        for row in &page2 {
            assert!(!page1_ids.contains(&row.id), "pages must not overlap");
        }
    }

    #[tokio::test]
    async fn test_get_leaderboard_ordered_by_count_desc() {
        let (pool, _pg) = setup_pg().await;
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let id3 = Uuid::new_v4();

        for _ in 0..3 {
            insert_like(&pool, Uuid::new_v4(), "post", id1)
                .await
                .unwrap();
        }
        for _ in 0..2 {
            insert_like(&pool, Uuid::new_v4(), "post", id2)
                .await
                .unwrap();
        }
        insert_like(&pool, Uuid::new_v4(), "post", id3)
            .await
            .unwrap();

        let rows = get_leaderboard(&pool, None, None, 10).await.unwrap();

        assert_eq!(rows.len(), 3);
        assert!(rows[0].2 >= rows[1].2 && rows[1].2 >= rows[2].2);
        assert_eq!(rows[0].2, 3);
    }

    #[tokio::test]
    async fn test_unique_constraint_enforced() {
        let (pool, _pg) = setup_pg().await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let (_, first) = insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();
        let (_, second) = insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();

        assert!(!first);
        assert!(second);

        let count = get_count(&pool, "post", content_id).await.unwrap();
        assert_eq!(count, 1, "duplicate like must not increment count");
    }
}
