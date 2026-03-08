use chrono::{DateTime, Utc};
use sqlx::{PgPool, QueryBuilder};
use uuid::Uuid;

/// Like record from the database.
/// Note: `user_id` is intentionally not included — callers always know the
/// user_id already (they pass it in). This avoids selecting an unused column.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct LikeRow {
    pub id: i64,
    pub content_type: String,
    pub content_id: Uuid,
    pub created_at: DateTime<Utc>,
}

/// Result of an insert_like operation.
/// Uses an enum to avoid fabricating synthetic timestamps for edge cases.
pub enum InsertLikeResult {
    /// New like was inserted. Contains the row and authoritative count.
    Inserted { row: LikeRow, count: i64 },
    /// Like already existed. Contains the existing row and authoritative count.
    AlreadyExisted { row: LikeRow, count: i64 },
    /// Extremely rare: INSERT saw a conflict but the row was concurrently deleted
    /// before the follow-up SELECT. Returns only the authoritative count (0).
    ConcurrentlyRemoved { count: i64 },
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
            Ok(InsertLikeResult::Inserted {
                row: like_row,
                count,
            })
        }
        None => {
            // Already existed — fetch existing row + count in a single query
            // inside the transaction. Eliminates the race window where a
            // concurrent unlike could delete the row between queries.
            let existing: Option<(i64, String, Uuid, DateTime<Utc>, i64)> = sqlx::query_as(
                r#"
                SELECT l.id, l.content_type, l.content_id, l.created_at,
                       COALESCE(lc.total_count, 0)
                FROM likes l
                LEFT JOIN like_counts lc
                    ON lc.content_type = l.content_type AND lc.content_id = l.content_id
                WHERE l.user_id = $1 AND l.content_type = $2 AND l.content_id = $3
                "#,
            )
            .bind(user_id)
            .bind(content_type)
            .bind(content_id)
            .fetch_optional(&mut *tx)
            .await?;

            tx.commit().await?;

            match existing {
                Some((id, ct, cid, created_at, count)) => Ok(InsertLikeResult::AlreadyExisted {
                    row: LikeRow {
                        id,
                        content_type: ct,
                        content_id: cid,
                        created_at,
                    },
                    count,
                }),
                None => {
                    // Row was concurrently deleted between the INSERT conflict and the
                    // follow-up SELECT. The transaction is already committed above so we
                    // use `pool` directly to read the authoritative count rather than
                    // returning a hard-coded 0 that would corrupt the response.
                    let count: i64 = sqlx::query_scalar(
                        r#"
                        SELECT COALESCE(total_count, 0) FROM like_counts
                        WHERE content_type = $1 AND content_id = $2
                        "#,
                    )
                    .bind(content_type)
                    .bind(content_id)
                    .fetch_optional(pool)
                    .await?
                    .unwrap_or(0);

                    Ok(InsertLikeResult::ConcurrentlyRemoved { count })
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

/// Batch get like counts for multiple content items in a single query.
/// Uses unnest + LEFT JOIN like_counts — one round-trip for all items.
pub async fn batch_get_counts(
    pool: &PgPool,
    items: &[(String, Uuid)],
) -> Result<Vec<(String, Uuid, i64)>, sqlx::Error> {
    if items.is_empty() {
        return Ok(vec![]);
    }

    let mut content_types = Vec::with_capacity(items.len());
    let mut content_ids = Vec::with_capacity(items.len());
    for (ct, cid) in items {
        content_types.push(ct.as_str());
        content_ids.push(*cid);
    }

    let rows: Vec<(String, Uuid, i64)> = sqlx::query_as(
        r#"
        SELECT req.content_type,
               req.content_id,
               COALESCE(lc.total_count, 0) AS total_count
        FROM unnest($1::text[], $2::uuid[]) AS req(content_type, content_id)
        LEFT JOIN like_counts lc
            ON lc.content_type = req.content_type
           AND lc.content_id = req.content_id
        "#,
    )
    .bind(&content_types)
    .bind(&content_ids)
    .fetch_all(pool)
    .await?;

    Ok(rows)
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

    let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "SELECT id, content_type, content_id, created_at FROM likes WHERE user_id = ",
    );
    qb.push_bind(user_id);

    if let Some(ct) = content_type_filter {
        qb.push(" AND content_type = ");
        qb.push_bind(ct);
    }

    if let (Some(ts), Some(id)) = (cursor_ts, cursor_id) {
        qb.push(" AND (created_at, id) < (");
        qb.push_bind(ts);
        qb.push(", ");
        qb.push_bind(id);
        qb.push(")");
    }

    qb.push(" ORDER BY created_at DESC, id DESC LIMIT ");
    qb.push_bind(fetch_limit);

    qb.build_query_as::<LikeRow>().fetch_all(pool).await
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
    let mut qb: QueryBuilder<sqlx::Postgres> = match since {
        Some(since_ts) => {
            // Time-windowed: aggregate from likes table
            let mut qb = QueryBuilder::new(
                "SELECT content_type, content_id, COUNT(*) as count FROM likes WHERE created_at >= ",
            );
            qb.push_bind(since_ts);
            if let Some(ct) = content_type {
                qb.push(" AND content_type = ");
                qb.push_bind(ct);
            }
            qb.push(" GROUP BY content_type, content_id ORDER BY count DESC LIMIT ");
            qb.push_bind(limit);
            qb
        }
        None => {
            // All-time: use pre-materialized like_counts table
            let mut qb = QueryBuilder::new(
                "SELECT content_type, content_id, total_count as count FROM like_counts",
            );
            if let Some(ct) = content_type {
                qb.push(" WHERE content_type = ");
                qb.push_bind(ct);
            }
            qb.push(" ORDER BY total_count DESC LIMIT ");
            qb.push_bind(limit);
            qb
        }
    };

    qb.build_query_as().fetch_all(pool).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// Return a PgPool connected to the shared Postgres container.
    async fn setup_pg() -> (sqlx::PgPool, crate::test_containers::TestScope) {
        let scope = crate::test_containers::isolated_scope().await;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&scope.database_url)
            .await
            .expect("connect test postgres");
        (pool, scope)
    }

    #[tokio::test]
    async fn test_insert_like_new() {
        let (pool, _scope) = setup_pg().await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let result = insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();

        match result {
            InsertLikeResult::Inserted { row, count } => {
                assert_eq!(row.content_type, "post");
                assert_eq!(row.content_id, content_id);
                assert_eq!(count, 1);
            }
            _ => panic!("expected Inserted variant"),
        }
    }

    #[tokio::test]
    async fn test_insert_like_idempotent() {
        let (pool, _scope) = setup_pg().await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();
        let result = insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();

        assert!(
            matches!(result, InsertLikeResult::AlreadyExisted { .. }),
            "second insert must report AlreadyExisted"
        );
    }

    #[tokio::test]
    async fn test_delete_like_existing() {
        let (pool, _scope) = setup_pg().await;
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
        let (pool, _scope) = setup_pg().await;
        let result = delete_like(&pool, Uuid::new_v4(), "post", Uuid::new_v4())
            .await
            .unwrap();
        assert!(!result.was_liked);
    }

    #[tokio::test]
    async fn test_get_count_after_likes() {
        let (pool, _scope) = setup_pg().await;
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
        let (pool, _scope) = setup_pg().await;
        let count = get_count(&pool, "post", Uuid::new_v4()).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_get_like_status_liked() {
        let (pool, _scope) = setup_pg().await;
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
        let (pool, _scope) = setup_pg().await;
        let ts = get_like_status(&pool, Uuid::new_v4(), "post", Uuid::new_v4())
            .await
            .unwrap();
        assert!(ts.is_none());
    }

    #[tokio::test]
    async fn test_batch_get_statuses() {
        let (pool, _scope) = setup_pg().await;
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
        let (pool, _scope) = setup_pg().await;
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
        let (pool, _scope) = setup_pg().await;
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
        let (pool, _scope) = setup_pg().await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let first = insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();
        let second = insert_like(&pool, user_id, "post", content_id)
            .await
            .unwrap();

        assert!(matches!(first, InsertLikeResult::Inserted { .. }));
        assert!(matches!(second, InsertLikeResult::AlreadyExisted { .. }));

        let count = get_count(&pool, "post", content_id).await.unwrap();
        assert_eq!(count, 1, "duplicate like must not increment count");
    }
}
