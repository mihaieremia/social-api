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
