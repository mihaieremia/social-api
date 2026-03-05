# Unit Test Coverage Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add meaningful unit test coverage using real infrastructure (testcontainers) and one targeted internal refactor — no `Arc<dyn>` in AppState, no mockall, no production architecture changes.

**Architecture:** Keep `CacheManager`, `LikeService`, and `AppState` fully concrete. Use `testcontainers` to spin up real Postgres + Redis containers for service and repository tests. The only production code change is `LikeService.content_validator: HttpContentValidator` → `Arc<dyn ContentValidator>` (already a trait, contained within `LikeService`, zero hot-path overhead since the service is constructed once at startup). `RateLimiter` becomes an internal trait in `rate_limit.rs` (not in AppState), implemented by `CacheManager`, enabling `check_rate_limit` to be tested with a tiny in-test stub.

**Tech Stack:** `testcontainers-modules 0.11` (postgres + redis images), `sqlx #[sqlx::test]` (built into sqlx 0.8, auto-spins Postgres container + runs migrations per test), `tokio::test` for service tests.

---

## What we are NOT doing

- No `Arc<dyn LikeServiceTrait>` in AppState
- No `Arc<dyn TokenValidator>` in AppState
- No `Arc<dyn RateLimiter>` in AppState
- No mockall
- No handler-level mock injection
- No changes to AppState struct or constructor

---

## Task 1: Add dev dependencies

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/social-api/Cargo.toml`

**Step 1: Add to workspace `[workspace.dependencies]` in `Cargo.toml`**

```toml
testcontainers = "0.23"
testcontainers-modules = { version = "0.11", features = ["postgres", "redis"] }
```

**Step 2: Add `[dev-dependencies]` section to `crates/social-api/Cargo.toml`**

```toml
[dev-dependencies]
testcontainers = { workspace = true }
testcontainers-modules = { workspace = true }
# Enable testcontainers feature in sqlx for #[sqlx::test] auto-provisioning
sqlx = { workspace = true, features = ["runtime-tokio-rustls", "postgres", "uuid", "chrono", "migrate", "testcontainers"] }
```

**Step 3: Verify compilation**

```bash
cargo check --workspace
```

Expected: no errors.

**Step 4: Commit**

```bash
git add Cargo.toml crates/social-api/Cargo.toml Cargo.lock
git commit -m "chore(deps): add testcontainers for container-based unit tests"
```

---

## Task 2: Add `Config::new_for_test()`

Tests need a `Config` that doesn't read env vars (the real constructor panics if `CONTENT_API_*_URL` is missing).

**Files:**
- Modify: `crates/social-api/src/config.rs`

**Step 1: Append to the `impl Config` block**

Find the closing `}` of `impl Config` and add before it:

```rust
/// Minimal config for unit tests — does NOT read environment variables.
#[cfg(test)]
pub fn new_for_test() -> Self {
    let mut content_api_urls = std::collections::HashMap::new();
    content_api_urls.insert("post".to_string(), "http://localhost:8081".to_string());
    content_api_urls.insert("bonus_hunter".to_string(), "http://localhost:8081".to_string());
    content_api_urls.insert("top_picks".to_string(), "http://localhost:8081".to_string());

    Self {
        http_port: 8080,
        database_url: "postgres://social:social_password@localhost:5432/social_api_test"
            .to_string(),
        read_database_url:
            "postgres://social:social_password@localhost:5432/social_api_test".to_string(),
        redis_url: "redis://localhost:6379".to_string(),
        content_api_urls,
        profile_api_url: "http://localhost:8081".to_string(),
        db_max_connections: 5,
        db_min_connections: 1,
        db_acquire_timeout_secs: 5,
        redis_pool_size: 5,
        rate_limit_write_per_minute: 30,
        rate_limit_read_per_minute: 1000,
        cache_ttl_like_counts_secs: 300,
        cache_ttl_content_validation_secs: 3600,
        cache_ttl_user_status_secs: 60,
        circuit_breaker_failure_threshold: 5,
        circuit_breaker_recovery_timeout_secs: 30,
        circuit_breaker_success_threshold: 2,
        leaderboard_refresh_interval_secs: 300,
        shutdown_timeout_secs: 30,
        sse_broadcast_capacity: 128,
    }
}
```

> **Note:** Check the actual field names in the `Config` struct definition and copy them exactly. Adjust if fields differ.

**Step 2: Verify**

```bash
cargo check --workspace
```

**Step 3: Commit**

```bash
git add crates/social-api/src/config.rs
git commit -m "test: add Config::new_for_test() to bootstrap tests without env vars"
```

---

## Task 3: Internal `RateLimiter` trait in `rate_limit.rs`

The only goal here is making `check_rate_limit` independently testable without a real Redis. The trait stays private inside `rate_limit.rs` — AppState does not change.

**Files:**
- Modify: `crates/social-api/src/middleware/rate_limit.rs`

**Step 1: Define a private trait and implement it for `CacheManager`**

Add at the top of `rate_limit.rs`, after existing imports:

```rust
/// Internal abstraction over the sliding-window Lua script.
/// Kept private — not exposed through AppState.
#[async_trait::async_trait]
trait SlidingWindow: Send + Sync {
    async fn check(&self, key: &str, limit: u64, window_secs: u64) -> RateLimitResult;
}

#[async_trait::async_trait]
impl SlidingWindow for CacheManager {
    async fn check(&self, key: &str, limit: u64, window_secs: u64) -> RateLimitResult {
        check_rate_limit_inner(self, key, limit, window_secs).await
    }
}
```

**Step 2: Rename `check_rate_limit` → `check_rate_limit_inner`, keep it taking `&CacheManager`**

The two public middleware fns (`write_rate_limit`, `read_rate_limit`) call `check_rate_limit_inner(state.cache(), ...)`. Nothing changes in their signatures or in AppState.

**Step 3: Add unit tests for `fnv1a_hash` and rate-limit logic**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // --- Pure function tests (no infrastructure needed) ---

    #[test]
    fn test_fnv1a_hash_is_deterministic() {
        assert_eq!(fnv1a_hash("tok_user_1"), fnv1a_hash("tok_user_1"));
        assert_ne!(fnv1a_hash("tok_user_1"), fnv1a_hash("tok_user_2"));
    }

    #[test]
    fn test_fnv1a_hash_different_inputs_differ() {
        let inputs = ["", "a", "ab", "abc", "192.168.1.1", "10.0.0.1"];
        let hashes: Vec<u64> = inputs.iter().map(|s| fnv1a_hash(s)).collect();
        // All hashes must be unique
        let unique: std::collections::HashSet<u64> = hashes.iter().copied().collect();
        assert_eq!(unique.len(), inputs.len());
    }

    #[test]
    fn test_add_rate_limit_headers_sets_correct_values() {
        let result = RateLimitResult {
            allowed: true,
            current: 5,
            limit: 30,
            reset_secs: 45,
        };
        let mut response = axum::response::Response::new(axum::body::Body::empty());
        add_rate_limit_headers(&mut response, &result);

        let headers = response.headers();
        assert_eq!(headers.get("X-RateLimit-Limit").unwrap(), "30");
        assert_eq!(headers.get("X-RateLimit-Remaining").unwrap(), "25");
        assert_eq!(headers.get("X-RateLimit-Reset").unwrap(), "45");
    }

    #[test]
    fn test_rate_limited_response_is_429() {
        let result = RateLimitResult {
            allowed: false,
            current: 30,
            limit: 30,
            reset_secs: 12,
        };
        let response = rate_limited_response(&result);
        assert_eq!(response.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().get("Retry-After").is_some());
    }

    // --- Redis Lua script test (testcontainers) ---

    #[tokio::test]
    async fn test_sliding_window_allows_under_limit() {
        let redis = testcontainers_modules::redis::Redis::default()
            .start().await.expect("redis container");
        let port = redis.get_host_port_ipv4(6379).await.unwrap();

        let mut config = crate::config::Config::new_for_test();
        config.redis_url = format!("redis://127.0.0.1:{port}");
        let pool = crate::cache::manager::create_pool(&config).await.unwrap();
        let cache = CacheManager::new(pool);

        let result = check_rate_limit_inner(&cache, "rl:test:allow", 10, 60).await;
        assert!(result.allowed);
        assert_eq!(result.current, 1);
        assert_eq!(result.limit, 10);
    }

    #[tokio::test]
    async fn test_sliding_window_blocks_over_limit() {
        let redis = testcontainers_modules::redis::Redis::default()
            .start().await.expect("redis container");
        let port = redis.get_host_port_ipv4(6379).await.unwrap();

        let mut config = crate::config::Config::new_for_test();
        config.redis_url = format!("redis://127.0.0.1:{port}");
        let pool = crate::cache::manager::create_pool(&config).await.unwrap();
        let cache = CacheManager::new(pool);

        // Exhaust the limit of 3
        for _ in 0..3 {
            check_rate_limit_inner(&cache, "rl:test:block", 3, 60).await;
        }

        let result = check_rate_limit_inner(&cache, "rl:test:block", 3, 60).await;
        assert!(!result.allowed);
        assert_eq!(result.current, 3);
    }

    #[tokio::test]
    async fn test_sliding_window_fails_open_on_redis_unavailable() {
        // Point at a port with no Redis — cache ops degrade gracefully
        let mut config = crate::config::Config::new_for_test();
        config.redis_url = "redis://127.0.0.1:19999".to_string();
        // bb8 pool with 0 retry so the test doesn't hang
        let manager = bb8_redis::RedisConnectionManager::new(config.redis_url.as_str()).unwrap();
        let pool = bb8::Pool::builder()
            .connection_timeout(std::time::Duration::from_millis(100))
            .build(manager)
            .await
            .unwrap();
        let cache = CacheManager::new(pool);

        let result = check_rate_limit_inner(&cache, "rl:test:failopen", 10, 60).await;
        // Must fail open (allow) — never drop legitimate traffic when Redis is down
        assert!(result.allowed);
    }
}
```

**Step 4: Run the new tests**

```bash
cargo test --lib -p social-api middleware::rate_limit -- --nocapture
```

Expected: all pass. The container tests require Docker.

**Step 5: Commit**

```bash
git add crates/social-api/src/middleware/rate_limit.rs
git commit -m "test: rate-limit unit tests (pure fns + Lua script via testcontainers Redis)"
```

---

## Task 4: Change `LikeService.content_validator` to `Arc<dyn ContentValidator>`

`LikeService` holds `HttpContentValidator` (concrete). Changing it to `Arc<dyn ContentValidator>` is a small, self-contained change that lets service tests inject a stub without starting a real HTTP server. `ContentValidator` is already defined as a trait. AppState does not change — it still constructs `LikeService::new(...)` the same way.

**Files:**
- Modify: `crates/social-api/src/services/like_service.rs`

**Step 1: Change the struct field**

```rust
pub struct LikeService {
    db: DbPools,
    cache: CacheManager,
    content_validator: std::sync::Arc<dyn crate::clients::content_client::ContentValidator>, // was HttpContentValidator
    config: Config,
    content_breaker: std::sync::Arc<CircuitBreaker>,
}
```

**Step 2: Update `LikeService::new()` — signature unchanged, only the internal construction changes**

```rust
pub fn new(
    db: DbPools,
    cache: CacheManager,
    http_client: reqwest::Client,
    config: Config,
    content_breaker: Arc<CircuitBreaker>,
) -> Self {
    let content_validator: Arc<dyn ContentValidator> = Arc::new(
        HttpContentValidator::new(http_client, cache.clone(), config.clone())
    );
    Self { db, cache, content_validator, config, content_breaker }
}
```

Everything that calls `LikeService::new(...)` (only `AppState::new()`) stays unchanged.

**Step 3: Verify**

```bash
cargo check --workspace
```

**Step 4: Commit**

```bash
git add crates/social-api/src/services/like_service.rs
git commit -m "refactor: LikeService uses Arc<dyn ContentValidator> internally for test injection"
```

---

## Task 5: Repository tests with `#[sqlx::test]`

`#[sqlx::test]` (built into sqlx 0.8) automatically:
1. Starts a Postgres container via testcontainers
2. Creates a fresh database per test
3. Runs all migrations from `./migrations/`
4. Passes a ready `PgPool` into the test function
5. Drops the database after the test

Zero manual setup. Tests are fully isolated.

**Files:**
- Modify: `crates/social-api/src/repositories/like_repository.rs`

**Step 1: Add the test module at the bottom of `like_repository.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;
    use uuid::Uuid;

    #[sqlx::test(migrations = "../../migrations")]
    async fn test_insert_like_new(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let (row, existed) = insert_like(&pool, user_id, "post", content_id).await.unwrap();

        assert!(!existed);
        assert_eq!(row.user_id, user_id);
        assert_eq!(row.content_type, "post");
        assert_eq!(row.content_id, content_id);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn test_insert_like_idempotent(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        insert_like(&pool, user_id, "post", content_id).await.unwrap();
        let (_, existed) = insert_like(&pool, user_id, "post", content_id).await.unwrap();

        assert!(existed, "second insert must report already_existed=true");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn test_delete_like_existing(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        insert_like(&pool, user_id, "post", content_id).await.unwrap();
        let was_liked = delete_like(&pool, user_id, "post", content_id).await.unwrap();

        assert!(was_liked);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn test_delete_like_not_existing(pool: PgPool) {
        let was_liked = delete_like(&pool, Uuid::new_v4(), "post", Uuid::new_v4())
            .await
            .unwrap();
        assert!(!was_liked);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn test_get_count_after_likes(pool: PgPool) {
        let content_id = Uuid::new_v4();
        for _ in 0..3 {
            insert_like(&pool, Uuid::new_v4(), "post", content_id).await.unwrap();
        }
        let count = get_count(&pool, "post", content_id).await.unwrap();
        assert_eq!(count, 3);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn test_get_count_zero_for_unknown_content(pool: PgPool) {
        let count = get_count(&pool, "post", Uuid::new_v4()).await.unwrap();
        assert_eq!(count, 0);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn test_get_like_status_liked(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        insert_like(&pool, user_id, "post", content_id).await.unwrap();
        let ts = get_like_status(&pool, user_id, "post", content_id).await.unwrap();

        assert!(ts.is_some(), "liked_at must be Some after liking");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn test_get_like_status_not_liked(pool: PgPool) {
        let ts = get_like_status(&pool, Uuid::new_v4(), "post", Uuid::new_v4())
            .await
            .unwrap();
        assert!(ts.is_none());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn test_batch_get_counts(pool: PgPool) {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        for _ in 0..3 {
            insert_like(&pool, Uuid::new_v4(), "post", id1).await.unwrap();
        }
        insert_like(&pool, Uuid::new_v4(), "post", id2).await.unwrap();

        let counts = batch_get_counts(&pool, &[
            ("post".to_string(), id1),
            ("post".to_string(), id2),
        ])
        .await
        .unwrap();

        let map: std::collections::HashMap<_, _> = counts
            .into_iter()
            .map(|(ct, cid, c)| ((ct, cid), c))
            .collect();

        assert_eq!(map[&("post".to_string(), id1)], 3);
        assert_eq!(map[&("post".to_string(), id2)], 1);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn test_batch_get_statuses(pool: PgPool) {
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

    #[sqlx::test(migrations = "../../migrations")]
    async fn test_get_user_likes_pagination(pool: PgPool) {
        let user_id = Uuid::new_v4();
        for _ in 0..5 {
            insert_like(&pool, user_id, "post", Uuid::new_v4()).await.unwrap();
        }

        // limit+1 fetch to detect has_more
        let page1 = get_user_likes(&pool, user_id, None, None, None, 2).await.unwrap();
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
        // None of page2's IDs should appear in page1
        let page1_ids: std::collections::HashSet<_> = page1.iter().map(|r| r.id).collect();
        for row in &page2 {
            assert!(!page1_ids.contains(&row.id), "pages must not overlap");
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn test_get_leaderboard_ordered_by_count_desc(pool: PgPool) {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let id3 = Uuid::new_v4();

        for _ in 0..3 { insert_like(&pool, Uuid::new_v4(), "post", id1).await.unwrap(); }
        for _ in 0..2 { insert_like(&pool, Uuid::new_v4(), "post", id2).await.unwrap(); }
        insert_like(&pool, Uuid::new_v4(), "post", id3).await.unwrap();

        let rows = get_leaderboard(&pool, None, None, 10).await.unwrap();

        assert_eq!(rows.len(), 3);
        // Rows must be descending by count
        assert!(rows[0].2 >= rows[1].2 && rows[1].2 >= rows[2].2);
        assert_eq!(rows[0].2, 3);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn test_unique_constraint_enforced(pool: PgPool) {
        // The unique constraint (user_id, content_type, content_id) is handled
        // by insert_like returning already_existed=true, not a DB error.
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        let (_, first) = insert_like(&pool, user_id, "post", content_id).await.unwrap();
        let (_, second) = insert_like(&pool, user_id, "post", content_id).await.unwrap();

        assert!(!first);
        assert!(second);

        let count = get_count(&pool, "post", content_id).await.unwrap();
        assert_eq!(count, 1, "duplicate like must not increment count");
    }
}
```

> **Note:** Adjust function signatures (`get_user_likes`, `get_leaderboard`, etc.) to match exactly what exists in `like_repository.rs`. The `pool: PgPool` arg from `#[sqlx::test]` can be passed directly if the repo fns take `&PgPool` — Rust auto-refs.

**Step 2: Run repository tests**

```bash
cargo test --lib -p social-api repositories -- --nocapture 2>&1 | tail -30
```

Expected: all pass. Requires Docker daemon running.

**Step 3: Commit**

```bash
git add crates/social-api/src/repositories/like_repository.rs
git commit -m "test: repository unit tests via sqlx::test (auto Postgres container + migrations)"
```

---

## Task 6: Service unit tests with testcontainers

Test `LikeService` business logic against real Redis + real Postgres. Only `ContentValidator` is stubbed (because it calls an external HTTP API that isn't available in unit tests).

**Files:**
- Modify: `crates/social-api/src/services/like_service.rs`

**Step 1: Add test helpers at the bottom of `like_service.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::{postgres::Postgres, redis::Redis};
    use uuid::Uuid;

    use crate::cache::manager::{CacheManager, create_pool};
    use crate::clients::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
    use crate::clients::content_client::ContentValidator;
    use crate::config::Config;
    use crate::db::DbPools;
    use shared::errors::AppError;

    // --- Content validator stubs ---

    struct AlwaysValid;
    #[async_trait::async_trait]
    impl ContentValidator for AlwaysValid {
        async fn validate(&self, _: &str, _: Uuid) -> Result<bool, AppError> { Ok(true) }
    }

    struct AlwaysNotFound;
    #[async_trait::async_trait]
    impl ContentValidator for AlwaysNotFound {
        async fn validate(&self, _: &str, _: Uuid) -> Result<bool, AppError> { Ok(false) }
    }

    // --- Container bootstrap helper ---

    struct TestInfra {
        pub service: LikeService,
        // Containers held here — dropped (stopped) when TestInfra is dropped
        _pg: testcontainers::ContainerAsync<Postgres>,
        _redis: testcontainers::ContainerAsync<Redis>,
    }

    async fn setup(validator: Arc<dyn ContentValidator>) -> TestInfra {
        let pg = Postgres::default().start().await.expect("postgres container");
        let redis = Redis::default().start().await.expect("redis container");

        let pg_port = pg.get_host_port_ipv4(5432).await.unwrap();
        let redis_port = redis.get_host_port_ipv4(6379).await.unwrap();

        let db_url = format!("postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres");

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&db_url)
            .await
            .expect("connect test postgres");

        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("run migrations");

        let db = DbPools { writer: pool.clone(), reader: pool };

        let mut config = Config::new_for_test();
        config.redis_url = format!("redis://127.0.0.1:{redis_port}");

        let redis_pool = create_pool(&config).await.expect("redis pool");
        let cache = CacheManager::new(redis_pool);

        let breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 5,
            recovery_timeout: std::time::Duration::from_secs(30),
            success_threshold: 2,
            service_name: "content_api_test".to_string(),
            rate_window: std::time::Duration::from_secs(30),
            failure_rate_threshold: 0.5,
            min_calls_for_rate: 10,
        }));

        let service = LikeService {
            db,
            cache,
            content_validator: validator,
            config,
            content_breaker: breaker,
        };

        TestInfra { service, _pg: pg, _redis: redis }
    }

    // --- Tests ---

    #[tokio::test]
    async fn test_like_new_content_returns_count_one() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let resp = infra.service
            .like(Uuid::new_v4(), "post", Uuid::new_v4())
            .await
            .unwrap();

        assert!(resp.liked);
        assert_eq!(resp.already_existed, Some(false));
        assert_eq!(resp.count, 1);
        assert!(resp.liked_at.is_some());
    }

    #[tokio::test]
    async fn test_like_same_content_twice_is_idempotent() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        infra.service.like(user_id, "post", content_id).await.unwrap();
        let resp = infra.service.like(user_id, "post", content_id).await.unwrap();

        assert_eq!(resp.already_existed, Some(true));
        assert_eq!(resp.count, 1, "count must not increase on duplicate");
    }

    #[tokio::test]
    async fn test_unlike_not_liked_returns_was_liked_false() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let resp = infra.service
            .unlike(Uuid::new_v4(), "post", Uuid::new_v4())
            .await
            .unwrap();

        assert!(!resp.liked);
        assert_eq!(resp.was_liked, Some(false));
    }

    #[tokio::test]
    async fn test_like_then_unlike_count_returns_to_zero() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        infra.service.like(user_id, "post", content_id).await.unwrap();
        let resp = infra.service.unlike(user_id, "post", content_id).await.unwrap();

        assert_eq!(resp.count, 0);
        assert_eq!(resp.was_liked, Some(true));
    }

    #[tokio::test]
    async fn test_get_count_cache_hit_returns_cached_value() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let content_id = Uuid::new_v4();

        // Pre-seed cache with a known value (bypassing DB entirely)
        let cache_key = format!("lc:post:{content_id}");
        infra.service.cache.set(&cache_key, "42", 300).await;

        let resp = infra.service.get_count("post", content_id).await.unwrap();
        assert_eq!(resp.count, 42, "must return cached value, not DB count of 0");
    }

    #[tokio::test]
    async fn test_get_count_cache_miss_falls_back_to_db() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let user_id = Uuid::new_v4();
        let content_id = Uuid::new_v4();

        // Like to create a DB record, clear cache to force DB fallback
        infra.service.like(user_id, "post", content_id).await.unwrap();
        infra.service.cache.del(&format!("lc:post:{content_id}")).await;

        let resp = infra.service.get_count("post", content_id).await.unwrap();
        assert_eq!(resp.count, 1, "must fall back to DB count");
    }

    #[tokio::test]
    async fn test_content_not_found_returns_error() {
        let infra = setup(Arc::new(AlwaysNotFound)).await;
        let err = infra.service
            .like(Uuid::new_v4(), "post", Uuid::new_v4())
            .await
            .unwrap_err();

        assert!(matches!(err, AppError::ContentNotFound { .. }));
    }

    #[tokio::test]
    async fn test_batch_counts_over_100_returns_error() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let items: Vec<(String, Uuid)> = (0..101)
            .map(|_| ("post".to_string(), Uuid::new_v4()))
            .collect();

        let err = infra.service.batch_counts(&items).await.unwrap_err();
        assert!(matches!(err, AppError::BatchTooLarge { size: 101, max: 100 }));
    }

    #[tokio::test]
    async fn test_batch_statuses_over_100_returns_error() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let items: Vec<(String, Uuid)> = (0..101)
            .map(|_| ("post".to_string(), Uuid::new_v4()))
            .collect();

        let err = infra.service
            .batch_statuses(Uuid::new_v4(), &items)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::BatchTooLarge { size: 101, max: 100 }));
    }

    #[tokio::test]
    async fn test_batch_counts_cache_miss_fetches_from_db() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let content_id = Uuid::new_v4();

        // Create 2 likes via the service (writes to DB + populates cache)
        for _ in 0..2 {
            infra.service.like(Uuid::new_v4(), "post", content_id).await.unwrap();
        }
        // Clear cache to force DB fallback in batch_counts
        infra.service.cache.del(&format!("lc:post:{content_id}")).await;

        let results = infra.service
            .batch_counts(&[("post".to_string(), content_id)])
            .await
            .unwrap();

        assert_eq!(results[0].count, 2);
    }

    #[tokio::test]
    async fn test_get_leaderboard_reads_from_zset_cache() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let content_id = Uuid::new_v4();

        // Seed leaderboard ZSET directly
        infra.service.cache
            .replace_sorted_set("lb:all", &[(format!("post:{content_id}"), 7.0)])
            .await;

        let resp = infra.service
            .get_leaderboard(None, shared::types::TimeWindow::All, 10)
            .await
            .unwrap();

        assert_eq!(resp.items.len(), 1);
        assert_eq!(resp.items[0].count, 7);
        assert_eq!(resp.items[0].content_id, content_id);
    }

    #[tokio::test]
    async fn test_get_leaderboard_falls_back_to_db_on_empty_zset() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let content_id = Uuid::new_v4();

        // Like 3 times — no ZSET seeded, so must fall back to DB
        for _ in 0..3 {
            infra.service.like(Uuid::new_v4(), "post", content_id).await.unwrap();
        }

        let resp = infra.service
            .get_leaderboard(None, shared::types::TimeWindow::All, 10)
            .await
            .unwrap();

        assert!(!resp.items.is_empty());
        assert_eq!(resp.items[0].count, 3);
    }

    #[tokio::test]
    async fn test_get_user_likes_cursor_pagination() {
        let infra = setup(Arc::new(AlwaysValid)).await;
        let user_id = Uuid::new_v4();

        for _ in 0..5 {
            infra.service.like(user_id, "post", Uuid::new_v4()).await.unwrap();
        }

        let page1 = infra.service
            .get_user_likes(user_id, None, None, 2)
            .await
            .unwrap();

        assert_eq!(page1.items.len(), 2);
        assert!(page1.has_more);
        assert!(page1.next_cursor.is_some());

        let page2 = infra.service
            .get_user_likes(user_id, None, page1.next_cursor.as_deref(), 2)
            .await
            .unwrap();

        assert!(!page2.items.is_empty());
        // No overlap between pages
        let page1_ids: std::collections::HashSet<_> =
            page1.items.iter().map(|i| i.content_id).collect();
        for item in &page2.items {
            assert!(!page1_ids.contains(&item.content_id));
        }
    }

    #[tokio::test]
    async fn test_circuit_breaker_open_returns_dependency_unavailable() {
        let infra = setup(Arc::new(AlwaysValid)).await;

        // Trip the breaker manually by recording failures
        for _ in 0..5 {
            infra.service.content_breaker.record_failure();
        }

        let err = infra.service
            .like(Uuid::new_v4(), "post", Uuid::new_v4())
            .await
            .unwrap_err();

        assert!(matches!(err, AppError::DependencyUnavailable(_)));
    }
}
```

**Step 2: Run service tests**

```bash
cargo test --lib -p social-api services::like_service::tests -- --nocapture 2>&1 | tail -40
```

Expected: all pass. Requires Docker.

**Step 3: Commit**

```bash
git add crates/social-api/src/services/like_service.rs
git commit -m "test: LikeService unit tests via testcontainers (real Redis + Postgres)"
```

---

## Task 7: Update Makefile

**Files:**
- Modify: `Makefile`

**Step 1: Update `test-unit` description and add a `test-unit-fast` variant**

```makefile
test-unit: ## Run unit tests — real infra via testcontainers (requires Docker)
	cargo test --workspace --lib

test-unit-fast: ## Run only pure unit tests — no containers, no Docker needed
	cargo test --workspace --lib \
	  -- --skip services::like_service::tests \
	     --skip repositories::like_repository::tests \
	     --skip middleware::rate_limit::tests::test_sliding_window
```

**Step 2: Verify help output**

```bash
make help
```

**Step 3: Verify tests run**

```bash
make test-unit
```

**Step 4: Commit**

```bash
git add Makefile
git commit -m "chore: update Makefile — test-unit uses testcontainers, test-unit-fast skips containers"
```

---

## Verification

After all 7 tasks complete:

```bash
make lint        # zero warnings
make test-unit   # all lib tests pass (Docker required)
make coverage    # rerun to see updated numbers
```

Expected coverage improvement:

| File | Before | After |
|---|---|---|
| `repositories/like_repository.rs` | 0% | ~85% |
| `services/like_service.rs` | 0% | ~65% |
| `middleware/rate_limit.rs` | 0% | ~55% |
| Overall | 11% | ~40%+ |
