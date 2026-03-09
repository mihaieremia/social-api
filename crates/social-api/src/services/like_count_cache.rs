use chrono::Utc;
use dashmap::DashMap;
use shared::errors::AppError;
use shared::types::BatchCountResult;
use tokio::sync::watch;
use uuid::Uuid;

use crate::cache::CacheManager;
use crate::config::Config;
use crate::db::DbPools;
use crate::repositories;

/// Like count cache with stampede coalescing.
///
/// On cache miss, uses a leader/follower pattern via `DashMap<String, watch::Sender>`:
/// the first task (leader) queries the DB and broadcasts the result; concurrent tasks
/// (followers) subscribe to the same `watch::channel` and receive it instantly.
/// If the leader fails or times out, followers fall back to their own DB query.
pub(crate) struct LikeCountCache {
    db: DbPools,
    cache: CacheManager,
    config: Config,
    pending_fetches: DashMap<String, watch::Sender<Option<i64>>>,
}

impl LikeCountCache {
    pub(crate) fn new(db: DbPools, cache: CacheManager, config: Config) -> Self {
        Self {
            db,
            cache,
            config,
            pending_fetches: DashMap::new(),
        }
    }

    /// Get the like count for a content item. Checks L1 → Redis → DB.
    /// Uses leader/follower coalescing to prevent thundering herd on cache miss.
    pub(crate) async fn get_count_value(
        &self,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<i64, AppError> {
        let cache_key = count_cache_key(content_type, content_id);

        if let Some(cached) = self.cache.get(&cache_key).await
            && let Ok(count) = cached.parse::<i64>()
        {
            return Ok(count);
        }

        enum FetchRole {
            Leader(watch::Sender<Option<i64>>),
            Follower(watch::Receiver<Option<i64>>),
        }

        let role = match self.pending_fetches.entry(cache_key.clone()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => {
                FetchRole::Follower(entry.get().subscribe())
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let (tx, _rx) = watch::channel(None::<i64>);
                entry.insert(tx.clone());
                FetchRole::Leader(tx)
            }
        };

        match role {
            FetchRole::Follower(mut rx) => {
                if let Some(count) = self.await_shared_count(&mut rx, &cache_key).await {
                    return Ok(count);
                }

                repositories::get_count(&self.db.reader, content_type, content_id)
                    .await
                    .map_err(map_db_error)
            }
            FetchRole::Leader(tx) => {
                let result =
                    repositories::get_count(&self.db.reader, content_type, content_id).await;

                match result {
                    Ok(count) => {
                        let _ = tx.send(Some(count));
                        self.pending_fetches.remove(&cache_key);
                        self.write_count(content_type, content_id, count).await;
                        Ok(count)
                    }
                    Err(error) => {
                        self.pending_fetches.remove(&cache_key);
                        Err(map_db_error(error))
                    }
                }
            }
        }
    }

    /// Batch get counts for up to 100 items. Uses MGET for cache hits,
    /// then coalesces DB fetches for misses using the same leader/follower pattern.
    pub(crate) async fn batch_counts(
        &self,
        items: &[(String, Uuid)],
    ) -> Result<Vec<BatchCountResult>, AppError> {
        if items.len() > 100 {
            return Err(AppError::BatchTooLarge {
                size: items.len(),
                max: 100,
            });
        }

        let cache_keys: Vec<_> = items
            .iter()
            .map(|(content_type, content_id)| count_cache_key(content_type, *content_id))
            .collect();
        let cached_values = self.cache.mget(&cache_keys).await;

        let mut results = Vec::with_capacity(items.len());
        let mut missing_map: std::collections::HashMap<String, Vec<usize>> =
            std::collections::HashMap::new();

        for (index, (content_type, content_id)) in items.iter().enumerate() {
            if let Some(Some(value)) = cached_values.get(index)
                && let Ok(count) = value.parse::<i64>()
            {
                results.push(BatchCountResult {
                    content_type: content_type.clone(),
                    content_id: *content_id,
                    count,
                });
                continue;
            }

            missing_map
                .entry(cache_keys[index].clone())
                .or_default()
                .push(index);
            results.push(BatchCountResult {
                content_type: content_type.clone(),
                content_id: *content_id,
                count: 0,
            });
        }

        if missing_map.is_empty() {
            return Ok(results);
        }

        let mut to_fetch = Vec::new();
        let mut leader_keys = Vec::new();
        let mut follower_rxs = Vec::new();

        for cache_key in missing_map.keys() {
            match self.pending_fetches.entry(cache_key.clone()) {
                dashmap::mapref::entry::Entry::Occupied(entry) => {
                    follower_rxs.push((cache_key.clone(), entry.get().subscribe()));
                }
                dashmap::mapref::entry::Entry::Vacant(entry) => {
                    let (tx, _rx) = watch::channel(None::<i64>);
                    entry.insert(tx);
                    let first_pos = missing_map[cache_key][0];
                    to_fetch.push(items[first_pos].clone());
                    leader_keys.push(cache_key.clone());
                }
            }
        }

        if !to_fetch.is_empty() {
            match repositories::batch_get_counts(&self.db.reader, &to_fetch).await {
                Ok(db_rows) => {
                    let count_map: std::collections::HashMap<(String, Uuid), i64> = db_rows
                        .into_iter()
                        .map(|(content_type, content_id, count)| {
                            ((content_type, content_id), count)
                        })
                        .collect();

                    let mut cache_entries = Vec::with_capacity(leader_keys.len());

                    for cache_key in &leader_keys {
                        let positions = &missing_map[cache_key];
                        let first_pos = positions[0];
                        let item = &items[first_pos];
                        let count = count_map.get(item).copied().unwrap_or(0);

                        for &pos in positions {
                            results[pos].count = count;
                        }

                        if let Some((_, tx)) = self.pending_fetches.remove(cache_key) {
                            let _ = tx.send(Some(count));
                        }

                        cache_entries.push((cache_key.clone(), count.to_string()));
                    }

                    let ttl = self.config.cache_ttl_like_counts_secs;
                    let set_entries: Vec<_> = cache_entries
                        .iter()
                        .map(|(key, value)| (key.as_str(), value.as_str(), ttl))
                        .collect();
                    self.cache.set_many(&set_entries).await;
                }
                Err(error) => {
                    for cache_key in &leader_keys {
                        self.pending_fetches.remove(cache_key);
                    }
                    return Err(map_db_error(error));
                }
            }
        }

        for (cache_key, mut rx) in follower_rxs {
            let count = if let Some(count) = self.await_shared_count(&mut rx, &cache_key).await {
                count
            } else if let Some(positions) = missing_map.get(&cache_key) {
                let first_pos = positions[0];
                let (content_type, content_id) = &items[first_pos];
                let count = repositories::get_count(&self.db.reader, content_type, *content_id)
                    .await
                    .map_err(map_db_error)?;
                self.write_count(content_type, *content_id, count).await;
                count
            } else {
                continue;
            };

            if let Some(positions) = missing_map.get(&cache_key) {
                for &pos in positions {
                    results[pos].count = count;
                }
            }
        }

        Ok(results)
    }

    /// Write-through: update the count in both L1 and Redis after a mutation.
    pub(crate) async fn write_count(&self, content_type: &str, content_id: Uuid, count: i64) {
        let cache_key = count_cache_key(content_type, content_id);
        self.cache
            .set(
                &cache_key,
                &count.to_string(),
                self.config.cache_ttl_like_counts_secs,
            )
            .await;
    }

    async fn await_shared_count(
        &self,
        rx: &mut watch::Receiver<Option<i64>>,
        cache_key: &str,
    ) -> Option<i64> {
        let timeout = std::time::Duration::from_secs(self.config.db_acquire_timeout_secs.max(1));
        match tokio::time::timeout(timeout, rx.changed()).await {
            Ok(Ok(())) => rx.borrow().as_ref().copied(),
            Ok(Err(_)) => None,
            Err(_) => {
                tracing::warn!(
                    cache_key,
                    timeout_secs = timeout.as_secs(),
                    observed_at = %Utc::now(),
                    "Timed out waiting for shared count fetch"
                );
                None
            }
        }
    }
}

pub(crate) fn map_db_error(error: sqlx::Error) -> AppError {
    AppError::Database(error.to_string())
}

fn count_cache_key(content_type: &str, content_id: Uuid) -> String {
    format!("lc:{content_type}:{content_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CacheManager;
    use crate::config::Config;
    use crate::db::DbPools;
    use crate::test_containers::isolated_scope;
    use shared::errors::ErrorCode;

    struct TestHarness {
        _scope: crate::test_containers::TestScope,
        cache_svc: LikeCountCache,
        db: DbPools,
    }

    async fn make_harness() -> TestHarness {
        let scope = isolated_scope().await;

        let mut config = Config::new_for_test();
        config.database_url = scope.database_url.clone();
        config.read_database_url = scope.database_url.clone();
        config.redis_url = scope.redis_url.clone();

        let db = DbPools::from_config(&config).await.expect("db pools");
        let redis_pool = crate::cache::create_pool(&config)
            .await
            .expect("redis pool");
        let cache = CacheManager::new(redis_pool, &config);
        let cache_svc = LikeCountCache::new(db.clone(), cache, config);

        TestHarness {
            _scope: scope,
            cache_svc,
            db,
        }
    }

    #[test]
    fn test_count_cache_key_format() {
        let uuid = Uuid::new_v4();
        let key = count_cache_key("article", uuid);
        assert_eq!(key, format!("lc:article:{uuid}"));
    }

    #[test]
    fn test_map_db_error_wraps_sqlx_error() {
        let sqlx_err = sqlx::Error::RowNotFound;
        let app_err = map_db_error(sqlx_err);
        assert!(matches!(app_err, AppError::Database(_)));
    }

    #[tokio::test]
    async fn test_batch_too_large_returns_error() {
        let harness = make_harness().await;

        let items: Vec<(String, Uuid)> = (0..101)
            .map(|_| ("article".to_string(), Uuid::new_v4()))
            .collect();

        let result = harness.cache_svc.batch_counts(&items).await;
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::BatchTooLarge);

        match err {
            AppError::BatchTooLarge { size, max } => {
                assert_eq!(size, 101);
                assert_eq!(max, 100);
            }
            other => panic!("Expected BatchTooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_write_count_and_get_count_value() {
        let harness = make_harness().await;
        let uuid = Uuid::new_v4();

        // Insert a row into like_counts so the DB has the record
        sqlx::query(
            "INSERT INTO like_counts (content_type, content_id, total_count) VALUES ($1, $2, $3)",
        )
        .bind("article")
        .bind(uuid)
        .bind(42_i64)
        .execute(&harness.db.writer)
        .await
        .expect("insert like_counts row");

        // Prime the cache via write_count
        harness.cache_svc.write_count("article", uuid, 42).await;

        // get_count_value should return the cached value
        let count = harness
            .cache_svc
            .get_count_value("article", uuid)
            .await
            .expect("get_count_value should succeed");

        assert_eq!(count, 42);
    }

    #[tokio::test]
    async fn test_get_count_value_cold_cache() {
        let harness = make_harness().await;
        let uuid = Uuid::new_v4();

        // Insert a row directly without warming the cache
        sqlx::query(
            "INSERT INTO like_counts (content_type, content_id, total_count) VALUES ($1, $2, $3)",
        )
        .bind("article")
        .bind(uuid)
        .bind(7_i64)
        .execute(&harness.db.writer)
        .await
        .expect("insert like_counts row");

        // get_count_value should fall through to DB on cache miss
        let count = harness
            .cache_svc
            .get_count_value("article", uuid)
            .await
            .expect("get_count_value should succeed on cold cache");

        assert_eq!(count, 7);
    }
}
