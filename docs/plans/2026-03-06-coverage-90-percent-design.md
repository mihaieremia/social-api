# Coverage 90% Design

**Date:** 2026-03-06

## Goal

Reach ≥90% line coverage on all testable code. Current baseline: 39.3% (1,281/3,260 lines). Adjusted target excludes bootstrap files that are genuinely untestable.

---

## Adjusted Coverage Target

**Exclude from 90% gate** (add to `--ignore-filename-regex` in Makefile):
```
crates/mock-services|src/main\.rs|src/logging\.rs|src/openapi\.rs
```

| | Lines |
|--|--|
| Total reportable (adjusted) | 3,154 |
| 90% target | 2,839 |
| Currently covered | 1,281 |
| Gap to close | 1,558 |

**Projected gap closure by source:**

| Source | New lines |
|--|--|
| In-process axum tests (HTTP layer) | ~645 |
| LikeService unit tests | ~364 |
| CacheManager unit tests | ~236 |
| PubsubManager tests | ~95 |
| Task tests (leaderboard, db_pool_metrics) | ~90 |
| Config env-var tests | ~80 |
| Rate limit + shared/errors + repository gaps | ~95 |
| **Total** | **~1,605** |

**Projected result:** 1,281 + 1,605 = **2,886 / 3,154 = 91.5%**

---

## Section 1: In-Process Axum Test Infrastructure

### Why

Integration tests hit the live Docker server (different process). Code running in the container contributes zero coverage to the test binary. Every handler, extractor, middleware, server.rs, state.rs, and db.rs is 0% for this reason.

### Approach

Build the full axum router in-process using `tower::ServiceExt`. Tests call handlers through the router without a network socket. Postgres + Redis provided by testcontainers (same as repository tests already do).

### New files

**`crates/social-api/tests/common/mod.rs`** — shared test infrastructure:

```rust
pub struct TestApp {
    pub router: Router,
    pub db: DbPools,
    pub cache: CacheManager,
}

impl TestApp {
    pub async fn new() -> Self {
        // spin up postgres + redis via testcontainers
        // build AppState with TestTokenValidator + TestContentValidator
        // return the full axum Router via server::build_router()
    }

    pub async fn request(&self, req: Request<Body>) -> Response<Body> {
        self.router.clone().oneshot(req).await.unwrap()
    }
}
```

**Two test doubles** (in `common/mod.rs`, ~10 lines each):

```rust
// Accepts any "Bearer <uuid>" → returns that UUID as user_id
struct TestTokenValidator;

#[async_trait]
impl TokenValidator for TestTokenValidator {
    async fn validate(&self, token: &str) -> Result<AuthenticatedUser, AppError> {
        let token = token.strip_prefix("Bearer ").unwrap_or(token);
        let user_id = Uuid::parse_str(token)
            .map_err(|_| AppError::Unauthorized("invalid token".into()))?;
        Ok(AuthenticatedUser { user_id })
    }
}

// Approves all content (type already validated by extractor)
struct TestContentValidator;

#[async_trait]
impl ContentValidator for TestContentValidator {
    async fn validate(&self, _content_type: &str, _content_id: Uuid) -> Result<bool, AppError> {
        Ok(true)
    }
}
```

**AppState injection** — `AppState::new_for_test(db, cache, token_validator, content_validator)` constructor that accepts trait objects. If fields are currently concrete types, wrap them in `Arc<dyn Trait>` for the test path.

### Test file: `crates/social-api/tests/http_test.rs`

One `TestApp` per test (testcontainers handles cleanup). Covers:

- Every endpoint: POST like, DELETE unlike, GET count, GET status, GET user-likes, POST batch-counts, POST batch-statuses, GET leaderboard, GET stream
- Every auth failure: missing header, invalid token format, non-UUID token
- Every extractor rejection: unknown content type, non-UUID content_id
- Every handler error branch: content not found (TestContentValidator returns false), batch too large, invalid cursor, invalid window
- Health endpoints: `/health/live`, `/health/ready`
- Metrics endpoint: `/metrics`

**What this covers simultaneously:**
- `server.rs` — router construction
- `state.rs` — AppState init
- `db.rs` — pool setup
- `middleware/request_id.rs` — X-Request-Id generation
- `middleware/error_context.rs` — patches request_id into error bodies
- `middleware/inflight.rs` — in-flight counter
- `middleware/metrics.rs` — HTTP metrics collection
- `extractors/auth.rs` — Bearer token → user_id
- `extractors/content_path.rs` — content type registry + UUID validation
- `handlers/likes.rs` — all like endpoints
- `handlers/health.rs` — liveness + readiness
- `handlers/stream.rs` — SSE
- `handlers/metrics_handler.rs` — Prometheus metrics

---

## Section 2: LikeService and CacheManager Unit Tests

### LikeService (`like_service.rs` — 404 lines at 0%)

Inline `#[cfg(test)]` module. Reuses testcontainers postgres + redis (same pattern as repository tests). Uses `TestContentValidator` for content validation injection.

Tests:
```
test_like_new_content                    // happy path, count incremented
test_like_duplicate_skips_validation     // already_existed=true, no Content API call
test_like_invalid_content_rolls_back     // TestContentValidator returns false → delete_like
test_unlike_was_liked                    // count decremented
test_unlike_not_liked                    // idempotent, no error
test_get_count_cache_hit                 // returns cached value, no DB query
test_get_count_cache_miss_populates      // DB queried, result cached
test_get_count_stampede_coalescing       // 10 concurrent callers → 1 DB query (watch channel)
test_batch_counts_partial_cache_hit      // mix of cached + uncached items
test_batch_counts_too_large              // >100 items → BatchTooLarge error
test_batch_statuses                      // returns correct liked/not-liked for user
test_get_leaderboard_cache_hit           // returns from Redis ZSET
test_get_leaderboard_db_fallback         // cold cache → DB query
test_get_user_likes_pagination           // cursor encode/decode roundtrip
test_content_breaker_open_rejects        // circuit open → DependencyUnavailable
```

### CacheManager (`cache/manager.rs` — 303 lines at 12%)

Inline `#[cfg(test)]` module. Uses testcontainers Redis. Tests every method individually:

```
test_get_set_del
test_get_missing_key_returns_none
test_conditional_incr_populates_missing_key
test_conditional_incr_existing_key
test_conditional_decr_does_not_go_negative
test_mget_mset_ex
test_set_nx_only_sets_once
test_set_nx_returns_false_on_conflict
test_publish_subscribe
test_zrevrange_with_scores
test_replace_sorted_set
test_redis_unavailable_returns_none      // error path → graceful degradation (None, not panic)
```

---

## Section 3: Config, Tasks, PubsubManager, and Remaining Gaps

### Config (`config.rs` — 149 lines at 36%)

No containers needed. Uses `std::env::set_var` / `temp_env` crate for hermetic env var tests:

```
test_required_var_missing_panics
test_optional_var_with_default
test_invalid_u64_falls_back_to_default
test_content_type_registry_built_from_env
test_new_for_test_returns_valid_config
```

### PubsubManager (`pubsub_manager.rs` — 105 lines at 0%)

Testcontainers Redis:
```
test_subscribe_receives_published_message
test_unsubscribe_stops_receiving
test_multiple_subscribers_same_channel
test_manager_shuts_down_cleanly
```

### Tasks (`leaderboard_refresh.rs` + `db_pool_metrics.rs` — 100 lines at 0%)

Testcontainers postgres + redis:
```
test_leaderboard_refresh_populates_redis_zset
test_leaderboard_refresh_runs_for_all_time_windows
test_db_pool_metrics_emits_gauges
```

### Remaining small gaps

- **`rate_limit.rs`** (78% → 90%): test the Lua Redis error path + `RateLimitConfig` builder branches
- **`shared/errors.rs`** (68% → 90%): cover `DependencyUnavailable`, `ContentNotFound`, `InvalidCursor` AppError variants
- **`like_repository.rs`** (85% → 90%): `get_leaderboard` with time window filter + `batch_get_counts` empty input edge case

---

## Makefile Changes

```makefile
COVERAGE_EXCLUDE := crates/mock-services|src/main\.rs|src/logging\.rs|src/openapi\.rs

coverage: ## Full coverage ≥90% (unit + integration; requires Docker + make up)
	@mkdir -p $(COVERAGE_DIR)
	cargo llvm-cov --workspace --all-targets \
	  --ignore-filename-regex '$(COVERAGE_EXCLUDE)' \
	  --lcov --output-path $(COVERAGE_DIR)/lcov.info \
	  -- --include-ignored --test-threads=1 \
	     --skip test_rate_limit_write_endpoint \
	     --skip test_circuit_breaker_trips_on_profile_api_failure
```

---

## Implementation Order

1. Makefile: update `COVERAGE_EXCLUDE`
2. `common/mod.rs`: `TestApp` + `TestTokenValidator` + `TestContentValidator`
3. `http_test.rs`: in-process axum tests (HTTP layer — biggest coverage gain)
4. `cache/manager.rs`: CacheManager unit tests
5. `like_service.rs`: LikeService unit tests
6. `pubsub_manager.rs`: PubsubManager tests
7. `tasks/`: leaderboard + db_pool_metrics tests
8. `config.rs`: env-var tests
9. Gap-fill: rate_limit, shared/errors, like_repository
10. Run `make coverage` — verify ≥90%
