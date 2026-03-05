# like_service + circuit_breaker Optimization Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix a TOCTOU race in the circuit breaker, replace sleep-based stampede protection with `tokio::sync::watch`, and clean up code quality issues in `like_service.rs`.

**Architecture:** Single `Mutex<Inner>` in `CircuitBreaker` eliminates all lock-ordering races. A `DashMap<key, watch::Sender>` in `LikeService` coalesces concurrent cache misses so only one DB query fires per cold key. Code quality pass deduplicates repeated patterns and fixes an O(n²) batch lookup.

**Tech Stack:** Rust, `tokio::sync::watch`, `dashmap`, `axum`, `sqlx`, `redis`

**Worktree:** `.worktrees/optimize-like-service-circuit-breaker`
**Branch:** `feat/optimize-like-service-circuit-breaker`

**Design doc:** `docs/plans/2026-03-06-like-service-circuit-breaker-optimization.md`

---

## Task 1: Add `dashmap` dependency

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/social-api/Cargo.toml`

**Step 1: Add to workspace Cargo.toml**

Open `Cargo.toml` (root). In `[workspace.dependencies]`, add:

```toml
dashmap = "6"
```

**Step 2: Add to crate Cargo.toml**

Open `crates/social-api/Cargo.toml`. In `[dependencies]`, add:

```toml
dashmap = { workspace = true }
```

**Step 3: Verify it compiles**

```bash
cargo build --workspace 2>&1 | tail -5
```

Expected: `Finished` with no errors.

**Step 4: Commit**

```bash
git add Cargo.toml crates/social-api/Cargo.toml Cargo.lock
git commit -m "chore(deps): add dashmap for stampede coalescing"
```

---

## Task 2: Refactor `circuit_breaker.rs` — single `Mutex<Inner>`

**Files:**
- Modify: `crates/social-api/src/clients/circuit_breaker.rs`

The current design uses four separate sync primitives (`RwLock<CircuitState>`, `AtomicU32` x2, `RwLock<Option<Instant>>`, `RwLock<VecDeque>`). This causes a TOCTOU race in `allow_request`: multiple threads can all read `should_try = true` and all race into `transition_to(HalfOpen)`.

Fix: collapse all mutable state into one `Mutex<Inner>`. Every method holds one lock for its full duration.

**Step 1: Verify existing tests pass before touching anything**

```bash
cargo test --workspace 2>&1 | tail -10
```

Expected: all 18 tests pass.

**Step 2: Replace the file content**

Rewrite `crates/social-api/src/clients/circuit_breaker.rs` with:

```rust
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Circuit breaker states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation — requests pass through
    Closed,
    /// Testing recovery — limited requests allowed
    HalfOpen,
    /// Failing fast — requests rejected immediately
    Open,
}

impl CircuitState {
    pub fn as_gauge_value(&self) -> f64 {
        match self {
            Self::Closed => 0.0,
            Self::HalfOpen => 1.0,
            Self::Open => 2.0,
        }
    }
}

/// Configuration for circuit breaker behavior.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Number of consecutive failures to trip the breaker
    pub failure_threshold: u32,
    /// Time to wait before transitioning from Open to HalfOpen
    pub recovery_timeout: Duration,
    /// Number of consecutive successes in HalfOpen to close the breaker
    pub success_threshold: u32,
    /// Service name for logging and metrics
    pub service_name: String,
    /// Sliding window duration for failure rate calculation
    pub rate_window: Duration,
    /// Failure rate threshold (0.0-1.0) to trip the breaker
    pub failure_rate_threshold: f64,
    /// Minimum calls in the window before rate-based tripping applies
    pub min_calls_for_rate: u32,
}

/// A single call outcome in the sliding window.
#[derive(Debug, Clone, Copy)]
struct CallRecord {
    timestamp: Instant,
    success: bool,
}

/// All mutable circuit breaker state under a single lock.
///
/// Using one `Mutex<Inner>` instead of multiple separate `RwLock`/`Atomic`
/// primitives eliminates TOCTOU races where threads could interleave reads
/// and writes across separate locks.
struct Inner {
    state: CircuitState,
    consecutive_failures: u32,
    consecutive_successes: u32,
    last_failure_time: Option<Instant>,
    /// Sliding window of recent call outcomes for failure rate calculation.
    call_window: VecDeque<CallRecord>,
}

/// Thread-safe circuit breaker state machine.
///
/// State transitions:
/// - Closed -> Open: after `failure_threshold` consecutive failures
///   **or** >50% failure rate in a 30-second window (configurable)
/// - Open -> HalfOpen: after `recovery_timeout` elapses
/// - HalfOpen -> Closed: after `success_threshold` consecutive successes
/// - HalfOpen -> Open: on any failure
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    inner: Mutex<Inner>,
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(Inner {
                state: CircuitState::Closed,
                consecutive_failures: 0,
                consecutive_successes: 0,
                last_failure_time: None,
                call_window: VecDeque::new(),
            }),
        }
    }

    /// Check if the circuit allows a request.
    ///
    /// The entire check-and-transition is performed under one lock hold,
    /// eliminating the TOCTOU race that existed when state was read under a
    /// read lock and then transitioned under a separate write lock.
    pub fn allow_request(&self) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match inner.state {
            CircuitState::Closed | CircuitState::HalfOpen => true,
            CircuitState::Open => {
                let elapsed = inner
                    .last_failure_time
                    .map(|t| t.elapsed() >= self.config.recovery_timeout)
                    .unwrap_or(false);
                if elapsed {
                    Self::do_transition(&self.config, &mut inner, CircuitState::HalfOpen);
                }
                elapsed
            }
        }
    }

    /// Record a successful request.
    pub fn record_success(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.consecutive_failures = 0;

        // No point tracking outcomes we're already rejecting.
        if inner.state != CircuitState::Open {
            Self::push_call(&self.config, &mut inner, true);
        }

        if inner.state == CircuitState::HalfOpen {
            inner.consecutive_successes += 1;
            if inner.consecutive_successes >= self.config.success_threshold {
                Self::do_transition(&self.config, &mut inner, CircuitState::Closed);
            }
        }
    }

    /// Record a failed request.
    pub fn record_failure(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.consecutive_successes = 0;
        inner.last_failure_time = Some(Instant::now());

        // No point tracking outcomes we're already rejecting.
        if inner.state != CircuitState::Open {
            Self::push_call(&self.config, &mut inner, false);
        }

        match inner.state {
            CircuitState::Closed => {
                inner.consecutive_failures += 1;
                if inner.consecutive_failures >= self.config.failure_threshold {
                    Self::do_transition(&self.config, &mut inner, CircuitState::Open);
                    return;
                }
                if Self::failure_rate_exceeded(&self.config, &inner) {
                    Self::do_transition(&self.config, &mut inner, CircuitState::Open);
                }
            }
            CircuitState::HalfOpen => {
                // Any failure in half-open goes back to open immediately.
                Self::do_transition(&self.config, &mut inner, CircuitState::Open);
            }
            CircuitState::Open => {
                // Already open — just keep counting.
                inner.consecutive_failures += 1;
            }
        }
    }

    /// Get current state (test-only).
    #[cfg(test)]
    pub fn state(&self) -> CircuitState {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).state
    }

    /// Push a call outcome into the sliding window, pruning expired entries.
    fn push_call(config: &CircuitBreakerConfig, inner: &mut Inner, success: bool) {
        let now = Instant::now();
        let cutoff = now - config.rate_window;
        while let Some(front) = inner.call_window.front() {
            if front.timestamp < cutoff {
                inner.call_window.pop_front();
            } else {
                break;
            }
        }
        inner.call_window.push_back(CallRecord {
            timestamp: now,
            success,
        });
    }

    /// Check if the failure rate in the sliding window exceeds the threshold.
    fn failure_rate_exceeded(config: &CircuitBreakerConfig, inner: &Inner) -> bool {
        let total = inner.call_window.len() as u32;
        if total < config.min_calls_for_rate {
            return false;
        }
        let failures = inner.call_window.iter().filter(|r| !r.success).count() as f64;
        failures / total as f64 > config.failure_rate_threshold
    }

    fn do_transition(config: &CircuitBreakerConfig, inner: &mut Inner, new_state: CircuitState) {
        let old_state = inner.state;
        if old_state == new_state {
            return;
        }

        inner.state = new_state;

        tracing::warn!(
            service = %config.service_name,
            from = ?old_state,
            to = ?new_state,
            "Circuit breaker state transition"
        );

        metrics::gauge!(
            "social_api_circuit_breaker_state",
            "service" => config.service_name.clone(),
        )
        .set(new_state.as_gauge_value());

        match new_state {
            CircuitState::Closed => {
                inner.consecutive_failures = 0;
                inner.consecutive_successes = 0;
                // Clear window on close so stale failures don't re-trip via rate check.
                inner.call_window.clear();
            }
            CircuitState::HalfOpen => {
                inner.consecutive_successes = 0;
            }
            CircuitState::Open => {
                inner.consecutive_successes = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: 3,
            recovery_timeout: Duration::from_millis(100),
            success_threshold: 2,
            service_name: "test".to_string(),
            rate_window: Duration::from_secs(30),
            failure_rate_threshold: 0.5,
            min_calls_for_rate: 10,
        }
    }

    #[test]
    fn test_closed_allows_requests() {
        let cb = CircuitBreaker::new(test_config());
        assert!(cb.allow_request());
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_opens_after_threshold_failures() {
        let cb = CircuitBreaker::new(test_config());
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Closed);
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
    }

    #[test]
    fn test_open_rejects_requests() {
        let cb = CircuitBreaker::new(test_config());
        for _ in 0..3 {
            cb.record_failure();
        }
        assert!(!cb.allow_request());
    }

    #[test]
    fn test_half_open_after_recovery_timeout() {
        let config = CircuitBreakerConfig {
            recovery_timeout: Duration::from_millis(10),
            ..test_config()
        };
        let cb = CircuitBreaker::new(config);
        for _ in 0..3 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);

        std::thread::sleep(Duration::from_millis(20));
        assert!(cb.allow_request());
        assert_eq!(cb.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn test_closes_after_success_threshold() {
        let config = CircuitBreakerConfig {
            recovery_timeout: Duration::from_millis(10),
            ..test_config()
        };
        let cb = CircuitBreaker::new(config);
        for _ in 0..3 {
            cb.record_failure();
        }
        std::thread::sleep(Duration::from_millis(20));
        cb.allow_request(); // Transition to HalfOpen

        cb.record_success();
        assert_eq!(cb.state(), CircuitState::HalfOpen);
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_half_open_failure_reopens() {
        let config = CircuitBreakerConfig {
            recovery_timeout: Duration::from_millis(10),
            ..test_config()
        };
        let cb = CircuitBreaker::new(config);
        for _ in 0..3 {
            cb.record_failure();
        }
        std::thread::sleep(Duration::from_millis(20));
        cb.allow_request(); // Transition to HalfOpen

        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
    }

    #[test]
    fn test_success_resets_failure_count() {
        let cb = CircuitBreaker::new(test_config());
        cb.record_failure();
        cb.record_failure();
        cb.record_success(); // Resets counter
        cb.record_failure();
        cb.record_failure();
        // Should still be closed (only 2 consecutive failures)
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_failure_rate_trips_breaker() {
        let config = CircuitBreakerConfig {
            failure_threshold: 100, // high so consecutive doesn't trip
            min_calls_for_rate: 10,
            failure_rate_threshold: 0.5,
            ..test_config()
        };
        let cb = CircuitBreaker::new(config);

        // 4 successes + 6 failures = 60% failure rate (>50%)
        for _ in 0..4 {
            cb.record_success();
        }
        for _ in 0..5 {
            cb.record_failure();
            assert_eq!(cb.state(), CircuitState::Closed); // not enough calls yet
        }
        // 10th call (failure) — now 6/10 = 60% > 50%
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
    }

    #[test]
    fn test_failure_rate_needs_minimum_calls() {
        let config = CircuitBreakerConfig {
            failure_threshold: 100,
            min_calls_for_rate: 10,
            failure_rate_threshold: 0.5,
            ..test_config()
        };
        let cb = CircuitBreaker::new(config);

        for _ in 0..5 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_window_cleared_on_close() {
        let config = CircuitBreakerConfig {
            failure_threshold: 3,
            recovery_timeout: Duration::from_millis(10),
            min_calls_for_rate: 10,
            ..test_config()
        };
        let cb = CircuitBreaker::new(config);

        for _ in 0..3 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);

        std::thread::sleep(Duration::from_millis(20));
        cb.allow_request(); // HalfOpen
        cb.record_success();
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);

        // Window should be cleared — old failures shouldn't cause rate trip
        let inner = cb.inner.lock().unwrap();
        assert!(inner.call_window.is_empty());
    }

    #[test]
    fn test_allow_request_atomic_no_double_transition() {
        // Verify that concurrent allow_request calls don't double-transition.
        // With a single Mutex, only one thread can execute the check+transition
        // atomically — the rest see HalfOpen already set.
        use std::sync::Arc;
        let config = CircuitBreakerConfig {
            recovery_timeout: Duration::from_millis(1),
            ..test_config()
        };
        let cb = Arc::new(CircuitBreaker::new(config));
        for _ in 0..3 {
            cb.record_failure();
        }
        std::thread::sleep(Duration::from_millis(5));

        let handles: Vec<_> = (0..10)
            .map(|_| {
                let cb = Arc::clone(&cb);
                std::thread::spawn(move || cb.allow_request())
            })
            .collect();

        let allowed: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // All should return true (HalfOpen allows), and state should be HalfOpen
        assert!(allowed.iter().all(|&a| a));
        assert_eq!(cb.state(), CircuitState::HalfOpen);
    }
}
```

**Step 3: Run tests**

```bash
cargo test --workspace 2>&1 | tail -15
```

Expected: all tests pass including the new `test_allow_request_atomic_no_double_transition`.

**Step 4: Commit**

```bash
git add crates/social-api/src/clients/circuit_breaker.rs
git commit -m "refactor(circuit-breaker): single Mutex<Inner> eliminates TOCTOU race

Replace four separate sync primitives (RwLock<state>, AtomicU32 x2,
RwLock<Instant>, RwLock<VecDeque>) with one Mutex<Inner>. Every
allow_request/record_success/record_failure now holds one lock for its
full duration — no interleaving between state read and write.

Also: unwrap_or_else(into_inner) instead of unwrap on all lock calls,
skip window updates when already Open, add concurrency regression test."
```

---

## Task 3: `like_service.rs` — code quality pass

**Files:**
- Modify: `crates/social-api/src/services/like_service.rs`

**Step 1: Add imports at the top of the file**

At the top of `like_service.rs`, update imports:

```rust
use std::sync::Arc;

use chrono::{Duration, Utc};
use dashmap::DashMap;
use shared::errors::AppError;
use shared::types::*;
use tokio::sync::watch;
use uuid::Uuid;

use crate::cache::manager::CacheManager;
use crate::clients::circuit_breaker::CircuitBreaker;
use crate::clients::content_client::{ContentValidator, HttpContentValidator};
use crate::config::Config;
use crate::db::DbPools;
use crate::repositories::like_repository;
```

**Step 2: Rewrite the `LikeService` struct and `new`**

Replace the struct and `new` with:

```rust
/// Core like service handling all like-related business logic.
pub struct LikeService {
    db: DbPools,
    cache: CacheManager,
    /// Trait object — swappable transport (HTTP today, gRPC tomorrow).
    content_validator: Arc<dyn ContentValidator>,
    config: Config,
    content_breaker: Arc<CircuitBreaker>,
    /// In-progress cache fetches, keyed by cache key.
    /// Used for stampede coalescing — see `get_count_inner`.
    pending_fetches: DashMap<String, watch::Sender<Option<i64>>>,
}

impl LikeService {
    pub fn new(
        db: DbPools,
        cache: CacheManager,
        http_client: reqwest::Client,
        config: Config,
        content_breaker: Arc<CircuitBreaker>,
    ) -> Self {
        let content_validator = Arc::new(HttpContentValidator::new(
            http_client,
            cache.clone(),
            config.clone(),
        ));
        Self {
            db,
            cache,
            content_validator,
            config,
            content_breaker,
            pending_fetches: DashMap::new(),
        }
    }
}
```

**Step 3: Add `db_err` helper and `update_count_cache` helper**

Add these two private helpers just before the `parse_leaderboard_member` function at the bottom of the `impl` block:

```rust
    /// Map a sqlx error to AppError::Database.
    /// Centralizes the repeated `.map_err(|e| AppError::Database(e.to_string()))` pattern.
    fn db_err(e: sqlx::Error) -> AppError {
        AppError::Database(e.to_string())
    }

    /// Apply a conditional INCR (delta=1) or DECR (delta=-1) to the count cache.
    /// Returns the new count, falling back to `db_count` on cache error.
    async fn update_count_cache(&self, key: &str, delta: i64, db_count: i64) -> i64 {
        let ttl = self.config.cache_ttl_like_counts_secs;
        let result = if delta > 0 {
            self.cache.conditional_incr(key, ttl, db_count).await
        } else {
            self.cache.conditional_decr(key, ttl, db_count).await
        };
        result.unwrap_or(db_count)
    }
```

**Step 4: Replace all `.map_err(|e| AppError::Database(e.to_string()))` with `Self::db_err`**

In every method (`like`, `unlike`, `get_count_inner`, `get_user_likes`, `batch_counts`, `batch_statuses`, `get_leaderboard`, `validate_content`), replace:

```rust
.map_err(|e| AppError::Database(e.to_string()))?
```

with:

```rust
.map_err(Self::db_err)?
```

**Step 5: Deduplicate `like` cache update**

In `like`, replace:

```rust
let count = if !already_existed {
    let cache_key = format!("lc:{content_type}:{content_id}");
    let db_count = like_repository::get_count(&self.db.reader, content_type, content_id)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    self.cache
        .conditional_incr(&cache_key, self.config.cache_ttl_like_counts_secs, db_count)
        .await
        .unwrap_or(db_count)
} else {
    self.get_count_inner(content_type, content_id).await?
};
```

with:

```rust
let count = if !already_existed {
    let cache_key = format!("lc:{content_type}:{content_id}");
    let db_count = like_repository::get_count(&self.db.reader, content_type, content_id)
        .await
        .map_err(Self::db_err)?;
    self.update_count_cache(&cache_key, 1, db_count).await
} else {
    self.get_count_inner(content_type, content_id).await?
};
```

**Step 6: Deduplicate `unlike` cache update**

In `unlike`, replace:

```rust
let count = if was_liked {
    let cache_key = format!("lc:{content_type}:{content_id}");
    let db_count = like_repository::get_count(&self.db.reader, content_type, content_id)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    self.cache
        .conditional_decr(&cache_key, self.config.cache_ttl_like_counts_secs, db_count)
        .await
        .unwrap_or(db_count)
} else {
    self.get_count_inner(content_type, content_id).await?
};
```

with:

```rust
let count = if was_liked {
    let cache_key = format!("lc:{content_type}:{content_id}");
    let db_count = like_repository::get_count(&self.db.reader, content_type, content_id)
        .await
        .map_err(Self::db_err)?;
    self.update_count_cache(&cache_key, -1, db_count).await
} else {
    self.get_count_inner(content_type, content_id).await?
};
```

**Step 7: Fix `batch_counts` O(n²) position lookup**

In `batch_counts`, after the `let mut missing_indices` block and before the DB fetch, replace the inner loop that searches for position:

Old code (in the "Update results" loop after `db_counts`):
```rust
for (ct, cid, count) in db_counts {
    if let Some(pos) = results
        .iter()
        .position(|r| r.content_type == ct && r.content_id == cid)
    {
        results[pos].count = count;
    }
    cache_entries.push((format!("lc:{ct}:{cid}"), count.to_string()));
}
```

New code — build an index map before the DB call:

```rust
// Build O(1) lookup index from (content_type, content_id) -> results position.
let index: std::collections::HashMap<(String, Uuid), usize> = items
    .iter()
    .enumerate()
    .map(|(i, (ct, cid))| ((ct.clone(), *cid), i))
    .collect();

let db_counts = like_repository::batch_get_counts(&self.db.reader, &missing_items)
    .await
    .map_err(Self::db_err)?;

let mut cache_entries = Vec::with_capacity(db_counts.len());
for (ct, cid, count) in db_counts {
    if let Some(&pos) = index.get(&(ct.clone(), cid)) {
        results[pos].count = count;
    }
    cache_entries.push((format!("lc:{ct}:{cid}"), count.to_string()));
}
```

Also remove the now-unused `let mut cache_entries = Vec::with_capacity(db_counts.len());` line that was declared earlier in the old code.

**Step 8: Build and verify**

```bash
cargo build --workspace 2>&1 | tail -5
```

Expected: `Finished` with no errors. Fix any type errors before proceeding.

**Step 9: Run tests**

```bash
cargo test --workspace 2>&1 | tail -10
```

Expected: all tests pass.

**Step 10: Commit**

```bash
git add crates/social-api/src/services/like_service.rs
git commit -m "refactor(like-service): trait object, db_err helper, dedup cache updates, O(n) batch index

- content_validator: Arc<dyn ContentValidator> for transport-swappability
- add pending_fetches: DashMap field (used in next commit)
- db_err() helper replaces 8x repeated map_err closures
- update_count_cache() helper deduplicates like/unlike cache update pattern
- batch_counts: HashMap index replaces O(n) .position() scan per item"
```

---

## Task 4: `like_service.rs` — skip content validation on duplicate likes

**Files:**
- Modify: `crates/social-api/src/services/like_service.rs`

**Step 1: Reorder `like` to insert before validate**

The current `like` method validates content first, then inserts. To skip validation for duplicates we must know `already_existed` before calling the Content API. Reorder so `insert_like` runs first.

Replace the body of `like` with:

```rust
pub async fn like(
    &self,
    user_id: Uuid,
    content_type: &str,
    content_id: Uuid,
) -> Result<LikeActionResponse, AppError> {
    // Insert like first to determine if this is a new or duplicate request.
    // For duplicates, we skip content validation (see below).
    let (like_row, already_existed) =
        like_repository::insert_like(&self.db.writer, user_id, content_type, content_id)
            .await
            .map_err(Self::db_err)?;

    if !already_existed {
        // Content validation is skipped for duplicate likes.
        // Rationale: if this like already exists, the content was valid at the time it was
        // first created. Re-validating on every duplicate request would fire a redundant
        // external HTTP call with no correctness benefit.
        //
        // On validation failure for a NEW like: we undo the insert by calling delete_like
        // so the DB stays consistent.
        if let Err(e) = self.validate_content(content_type, content_id).await {
            // Best-effort rollback — if this fails, the orphaned row will have count=1
            // but no valid content. Acceptable: content APIs are expected to be available.
            let _ = like_repository::delete_like(
                &self.db.writer,
                user_id,
                content_type,
                content_id,
            )
            .await;
            return Err(e);
        }
    }

    let count = if !already_existed {
        let cache_key = format!("lc:{content_type}:{content_id}");
        let db_count = like_repository::get_count(&self.db.reader, content_type, content_id)
            .await
            .map_err(Self::db_err)?;
        self.update_count_cache(&cache_key, 1, db_count).await
    } else {
        self.get_count_inner(content_type, content_id).await?
    };

    if !already_existed {
        let event = serde_json::json!({
            "event": "like",
            "user_id": user_id,
            "count": count,
            "timestamp": like_row.created_at.to_rfc3339(),
        });
        let channel = format!("sse:{content_type}:{content_id}");
        self.cache.publish(&channel, &event.to_string()).await;
    }

    Ok(LikeActionResponse {
        liked: true,
        already_existed: Some(already_existed),
        was_liked: None,
        count,
        liked_at: Some(like_row.created_at),
    })
}
```

**Step 2: Build**

```bash
cargo build --workspace 2>&1 | tail -5
```

Expected: `Finished` with no errors.

**Step 3: Run tests**

```bash
cargo test --workspace 2>&1 | tail -10
```

Expected: all tests pass.

**Step 4: Commit**

```bash
git add crates/social-api/src/services/like_service.rs
git commit -m "perf(like-service): skip content validation on duplicate likes

Insert before validate so we know already_existed before hitting Content API.
Duplicate likes (already_existed=true) skip the external HTTP round-trip entirely.
On validation failure for a new like, delete_like rolls back the insert."
```

---

## Task 5: `like_service.rs` — stampede protection with `tokio::sync::watch`

**Files:**
- Modify: `crates/social-api/src/services/like_service.rs`

**Step 1: Replace `get_count_inner`**

Replace the entire `get_count_inner` method with the `watch`-based implementation:

```rust
/// Internal count getter with cache-first strategy and stampede protection.
///
/// On a cache miss, the first task to notice registers a `watch::Sender` in
/// `pending_fetches` and fetches from DB. Every concurrent waiter subscribes
/// to that sender and is woken immediately when the result is ready — no
/// fixed `sleep()` delay. If the fetcher fails, the sender drops, waiters
/// receive `Err` on `changed()` and fall back to DB directly.
async fn get_count_inner(&self, content_type: &str, content_id: Uuid) -> Result<i64, AppError> {
    let cache_key = format!("lc:{content_type}:{content_id}");

    // Fast path: cache hit
    if let Some(cached) = self.cache.get(&cache_key).await
        && let Ok(count) = cached.parse::<i64>()
    {
        return Ok(count);
    }

    // Stampede protection: if another task is already fetching this key,
    // subscribe to its result instead of firing a duplicate DB query.
    if let Some(sender) = self.pending_fetches.get(&cache_key) {
        let mut rx = sender.subscribe();
        // Release the DashMap shard lock before awaiting — holding it across
        // an await point would block other tasks from reading the same shard.
        drop(sender);
        rx.changed().await.ok();
        if let Some(count) = *rx.borrow() {
            return Ok(count);
        }
        // Sender was dropped (fetcher task failed) — fall through to DB directly.
    }

    // We are the designated fetcher. Register a channel so concurrent
    // waiters can subscribe to our result.
    let (tx, _rx) = watch::channel(None::<i64>);
    self.pending_fetches.insert(cache_key.clone(), tx.clone());

    let count = like_repository::get_count(&self.db.reader, content_type, content_id)
        .await
        .map_err(Self::db_err)?;

    self.cache
        .set(
            &cache_key,
            &count.to_string(),
            self.config.cache_ttl_like_counts_secs,
        )
        .await;

    // Wake all waiters immediately with the fetched count.
    tx.send(Some(count)).ok();
    self.pending_fetches.remove(&cache_key);

    Ok(count)
}
```

**Step 2: Remove the now-unused Redis lock helpers from `get_count_inner`**

The old implementation used `self.cache.set_nx` and `self.cache.del` for lock management. Verify no other method calls those for stampede purposes (they may still be used elsewhere — do not remove from `CacheManager`). Only remove the calls that were in `get_count_inner`.

**Step 3: Build**

```bash
cargo build --workspace 2>&1 | tail -5
```

Expected: `Finished` with no errors.

**Step 4: Run tests**

```bash
cargo test --workspace 2>&1 | tail -10
```

Expected: all tests pass.

**Step 5: Commit**

```bash
git add crates/social-api/src/services/like_service.rs
git commit -m "perf(like-service): replace sleep-based stampede protection with watch coalescing

On cache miss, first task fetches DB and broadcasts via watch::Sender stored
in pending_fetches DashMap. Concurrent waiters subscribe and wake instantly
on result — eliminating the fixed 50ms sleep(). If the fetcher fails, sender
drops, waiters fall through to DB directly (safe degradation preserved)."
```

---

## Task 6: Final verification

**Step 1: Full workspace build**

```bash
cargo build --workspace 2>&1 | tail -5
```

Expected: `Finished` with no errors, zero warnings about unused imports.

**Step 2: Run all tests**

```bash
cargo test --workspace 2>&1 | tail -15
```

Expected: all tests pass.

**Step 3: Clippy**

```bash
cargo clippy --workspace -- -D warnings 2>&1 | tail -20
```

Fix any clippy errors before proceeding.

**Step 4: Verify design doc is committed**

```bash
git log --oneline -8
```

Expected: see all task commits plus the design doc commit.

**Step 5: Done — hand off to finishing-a-development-branch**

Use `superpowers:finishing-a-development-branch` to merge, clean up the worktree, and open a PR.
