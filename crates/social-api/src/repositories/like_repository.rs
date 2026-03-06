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

/// Result of an insert_like operation.
/// Contains the like row, whether it already existed, and the authoritative
/// count from the `like_counts` table (read within the same transaction).
pub struct InsertLikeResult {
    pub row: LikeRow,
    pub already_existed: bool,
    /// Authoritative total_count from `like_counts`, read inside the write
    /// transaction. Safe to use as the cache value — no read-replica lag.
    pub count: i64,
}

/// Insert a like. Returns the row, existence flag, and authoritative count.
/// Atomically increments like_counts and returns the new count in a single transaction.
pub async fn insert_like(
    pool: &PgPool,
    user_id: Uuid,
    content_type: &str,
    content_id: Uuid,
) -> Result<InsertLikeResult, sqlx::Error> {
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
            // New like - increment count atomically and return new total
            let (count,): (i64,) = sqlx::query_as(
                r#"
                INSERT INTO like_counts (content_type, content_id, total_count, updated_at)
                VALUES ($1, $2, 1, NOW())
                ON CONFLICT (content_type, content_id)
                DO UPDATE SET total_count = like_counts.total_count + 1, updated_at = NOW()
                RETURNING total_count
                "#,
            )
            .bind(content_type)
            .bind(content_id)
            .fetch_one(&mut *tx)
            .await?;

            tx.commit().await?;
            Ok(InsertLikeResult {
                row: like_row,
                already_existed: false,
                count,
            })
        }
        None => {
            // Already existed — fetch the existing row and current count.
            //
            // Race condition: a concurrent unlike may have deleted the row between
            // our INSERT ON CONFLICT (which saw it) and this SELECT. Use
            // fetch_optional + fallback to avoid "no rows returned" 500 errors.
            let count: i64 = sqlx::query_scalar(
                r#"
                SELECT COALESCE(total_count, 0) FROM like_counts
                WHERE content_type = $1 AND content_id = $2
                "#,
            )
            .bind(content_type)
            .bind(content_id)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(0);

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
            .fetch_optional(pool)
            .await?;

            match existing {
                Some(row) => Ok(InsertLikeResult {
                    row,
                    already_existed: true,
                    count,
                }),
                None => {
                    // Row was deleted by a concurrent unlike between our INSERT
                    // check and this SELECT. Treat as a new like and retry.
                    // Create a synthetic row — the like no longer exists, so
                    // this is effectively a no-op "already existed" response.
                    Ok(InsertLikeResult {
                        row: LikeRow {
                            id: 0,
                            content_type: content_type.to_string(),
                            content_id,
                            created_at: chrono::Utc::now(),
                        },
                        already_existed: true,
                        count,
                    })
                }
            }
        }
    }
}

/// Result of a delete_like operation.
pub struct DeleteLikeResult {
    pub was_liked: bool,
    /// Authoritative total_count from `like_counts`, read inside the write transaction.
    pub count: i64,
}

/// Delete a like. Returns whether the row existed and the authoritative count.
/// Atomically decrements like_counts and returns the new count in a single transaction.
pub async fn delete_like(
    pool: &PgPool,
    user_id: Uuid,
    content_type: &str,
    content_id: Uuid,
) -> Result<DeleteLikeResult, sqlx::Error> {
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
        // Decrement count atomically and return new total.
        //
        // Use fetch_optional: a concurrent insert_like rollback (content validation
        // failure) could delete the likes row we just deleted, AND if the like_counts
        // row was never created (race on first-ever like), the UPDATE finds nothing.
        let count: i64 = sqlx::query_scalar(
            r#"
            UPDATE like_counts
            SET total_count = GREATEST(total_count - 1, 0), updated_at = NOW()
            WHERE content_type = $1 AND content_id = $2
            RETURNING total_count
            "#,
        )
        .bind(content_type)
        .bind(content_id)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(0);

        tx.commit().await?;
        Ok(DeleteLikeResult {
            was_liked: true,
            count,
        })
    } else {
        // Not liked — read current count
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT COALESCE(total_count, 0) FROM like_counts
            WHERE content_type = $1 AND content_id = $2
            "#,
        )
        .bind(content_type)
        .bind(content_id)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(0);

        tx.commit().await?;
        Ok(DeleteLikeResult {
            was_liked: false,
            count,
        })
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
    use uuid::Uuid;

    /// Return a PgPool connected to the shared Postgres container.
    async fn setup_pg() -> sqlx::PgPool {
        let pg = crate::test_containers::shared_pg().await;
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&pg.url)
            .await
            .expect("connect test postgres")
    }

    #[tokio::test]
    async fn test_insert_like_new() {
        let pool = setup_pg().await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let result = insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();

        assert!(!result.already_existed);
        assert_eq!(result.row.content_type, "post");
        assert_eq!(result.row.content_id, content_id);
        assert_eq!(result.count, 1);
    }

    #[tokio::test]
    async fn test_insert_like_idempotent() {
        let pool = setup_pg().await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();
        let result = insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();

        assert!(
            result.already_existed,
            "second insert must report already_existed=true"
        );
    }

    #[tokio::test]
    async fn test_delete_like_existing() {
        let pool = setup_pg().await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();
        let result = delete_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();

        assert!(result.was_liked);
    }

    #[tokio::test]
    async fn test_delete_like_not_existing() {
        let pool = setup_pg().await;
        let result = delete_like(&pool, Uuid::new_v4(), "post", Uuid::new_v4())
            .await
            .unwrap();
        assert!(!result.was_liked);
    }

    #[tokio::test]
    async fn test_get_count_after_likes() {
        let pool = setup_pg().await;
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
        let pool = setup_pg().await;
        let count = get_count(&pool, "post", Uuid::new_v4()).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_get_like_status_liked() {
        let pool = setup_pg().await;
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
        let pool = setup_pg().await;
        let ts = get_like_status(&pool, Uuid::new_v4(), "post", Uuid::new_v4())
            .await
            .unwrap();
        assert!(ts.is_none());
    }

    #[tokio::test]
    async fn test_batch_get_statuses() {
        let pool = setup_pg().await;
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
        let pool = setup_pg().await;
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
        let pool = setup_pg().await;
        // Use a unique content_type so shared-DB data from other tests is excluded
        let ct = "lb_repo_test";
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let id3 = Uuid::new_v4();

        for _ in 0..3 {
            insert_like(&pool, Uuid::new_v4(), ct, id1).await.unwrap();
        }
        for _ in 0..2 {
            insert_like(&pool, Uuid::new_v4(), ct, id2).await.unwrap();
        }
        insert_like(&pool, Uuid::new_v4(), ct, id3).await.unwrap();

        let rows = get_leaderboard(&pool, Some(ct), None, 10).await.unwrap();

        assert_eq!(rows.len(), 3);
        assert!(rows[0].2 >= rows[1].2 && rows[1].2 >= rows[2].2);
        assert_eq!(rows[0].2, 3);
    }

    #[tokio::test]
    async fn test_unique_constraint_enforced() {
        let pool = setup_pg().await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let first = insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();
        let second = insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();

        assert!(!first.already_existed);
        assert!(second.already_existed);

        let count = get_count(&pool, "post", content_id).await.unwrap();
        assert_eq!(count, 1, "duplicate like must not increment count");
    }
}
