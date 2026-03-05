# like_service + circuit_breaker Optimization Design

**Date:** 2026-03-06
**Files:** `crates/social-api/src/services/like_service.rs`, `crates/social-api/src/clients/circuit_breaker.rs`

## Problem Statement

Three classes of issues identified:

1. **Correctness** — circuit breaker has a TOCTOU race across separate locks; panics possible on poisoned locks
2. **Performance** — stampede protection uses `sleep(50ms)` blocking the executor; `batch_counts` has O(n²) position lookup
3. **Code quality** — duplicated cache-update pattern in `like`/`unlike`; concrete type instead of trait object; repeated `.map_err`; content validation on duplicate likes

---

## Section 1: `like_service.rs` Structural Changes

### Struct

Add `pending_fetches` for stampede coalescing. Swap `HttpContentValidator` for a trait object:

```rust
pub struct LikeService {
    db: DbPools,
    cache: CacheManager,
    content_validator: Arc<dyn ContentValidator>,   // trait object — swappable (HTTP today, gRPC tomorrow)
    config: Config,
    content_breaker: Arc<CircuitBreaker>,
    pending_fetches: DashMap<String, watch::Sender<Option<i64>>>,  // stampede coalescing map
}
```

### Skip content validation on duplicate likes

```rust
// Content validation is skipped for duplicate likes.
// Rationale: if this like already exists, the content was valid at first-like time.
// Re-validating on every duplicate would fire a redundant external HTTP call with no safety benefit.
if !already_existed {
    self.validate_content(content_type, content_id).await?;
}
```

### Deduplicate cache update logic

Extract a private helper used by both `like` and `unlike`, eliminating the copy-paste:

```rust
async fn update_count_cache(&self, key: &str, delta: i64, db_count: i64) -> i64 {
    match delta {
        1 => self.cache.conditional_incr(key, self.config.cache_ttl_like_counts_secs, db_count).await,
        _ => self.cache.conditional_decr(key, self.config.cache_ttl_like_counts_secs, db_count).await,
    }
    .unwrap_or(db_count)
}
```

### Fix `batch_counts` O(n²) lookup

Pre-build a `HashMap<(String, Uuid), usize>` index before the DB loop, replacing the `.position()` linear scan per item:

```rust
let mut index: HashMap<(String, Uuid), usize> = items
    .iter()
    .enumerate()
    .map(|(i, (ct, cid))| ((ct.clone(), *cid), i))
    .collect();

for (ct, cid, count) in db_counts {
    if let Some(&pos) = index.get(&(ct.clone(), cid)) {
        results[pos].count = count;
    }
    cache_entries.push((format!("lc:{ct}:{cid}"), count.to_string()));
}
```

### Reduce `.map_err` repetition

Add a module-level helper:

```rust
fn db_err(e: sqlx::Error) -> AppError { AppError::Database(e.to_string()) }
```

Used as `.map_err(db_err)?` throughout.

---

## Section 2: Stampede Protection with `tokio::sync::watch`

Replace the Redis `SET NX` + `sleep(50ms)` pattern in `get_count_inner` with watch-channel coalescing.

### How it works

- On cache miss, check `pending_fetches` for an in-progress fetch on the same key
- If found: subscribe to its `watch::Receiver` and `await` the result — zero sleep, instant wake
- If not found: insert a `watch::Sender` into the map, fetch DB, broadcast result, remove entry
- If the fetcher errors: `tx` drops, `rx.changed()` returns `Err` — waiters fall through to DB directly (safe degradation)

```rust
async fn get_count_inner(&self, content_type: &str, content_id: Uuid) -> Result<i64, AppError> {
    let cache_key = format!("lc:{content_type}:{content_id}");

    // Fast path: cache hit
    if let Some(cached) = self.cache.get(&cache_key).await
        && let Ok(count) = cached.parse::<i64>()
    {
        return Ok(count);
    }

    // Stampede protection: if another task is already fetching this key, subscribe to its
    // result rather than firing a duplicate DB query.
    if let Some(sender) = self.pending_fetches.get(&cache_key) {
        let mut rx = sender.subscribe();
        drop(sender); // release DashMap shard lock before awaiting
        rx.changed().await.ok();
        if let Some(count) = *rx.borrow() {
            return Ok(count);
        }
        // Sender dropped (fetcher failed) — fall through to DB
    }

    // We are the designated fetcher — register a channel so concurrent waiters subscribe
    let (tx, _rx) = watch::channel(None::<i64>);
    self.pending_fetches.insert(cache_key.clone(), tx.clone());

    let count = like_repository::get_count(&self.db.reader, content_type, content_id)
        .await
        .map_err(db_err)?;

    self.cache
        .set(&cache_key, &count.to_string(), self.config.cache_ttl_like_counts_secs)
        .await;
    tx.send(Some(count)).ok(); // wake all waiters instantly
    self.pending_fetches.remove(&cache_key);

    Ok(count)
}
```

### Why `watch` over `SET NX` + sleep

| | `SET NX` + sleep | `watch` channel |
|---|---|---|
| Wake latency | ~50ms fixed | Immediate |
| Executor blocking | Parks 499 tasks for 50ms | Zero sleep, tasks yield until woken |
| External dependency | Redis round-trip for lock | In-process only |
| Degradation on fetcher failure | Falls back to DB | Falls back to DB |

---

## Section 3: `circuit_breaker.rs` Single `Mutex` Refactor

### Problem

Current design uses `RwLock<CircuitState>` + `AtomicU32` (failures/successes) + `RwLock<Option<Instant>>` + `RwLock<VecDeque<CallRecord>>` — four separate synchronization primitives. `allow_request` reads state under a read lock then calls `transition_to` which acquires a write lock — N threads can all see `should_try = true` simultaneously and all race into `transition_to(HalfOpen)`.

### Solution: `Mutex<Inner>`

Collapse all mutable state into a single struct under one `Mutex`:

```rust
struct Inner {
    state: CircuitState,
    consecutive_failures: u32,
    consecutive_successes: u32,
    last_failure_time: Option<Instant>,
    call_window: VecDeque<CallRecord>,
}

pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    inner: Mutex<Inner>,
}
```

Every operation acquires one lock for its full duration — no TOCTOU, no interleaving.

### `allow_request` — atomic check + transition

```rust
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
                self.do_transition(&mut inner, CircuitState::HalfOpen);
            }
            elapsed
        }
    }
}
```

Only one thread can ever win the `HalfOpen` transition — the rest see the state already changed.

### Poison handling

`.unwrap_or_else(|e| e.into_inner())` throughout — recovers the inner state rather than panicking on a poisoned lock.

### Skip window updates when `Open`

```rust
// No point tracking outcomes we're already rejecting — skip window update in Open state.
if inner.state != CircuitState::Open {
    push_call_inner(&mut inner, success, &self.config);
}
```

### Trade-offs

| | Multi-lock (current) | Single `Mutex` |
|---|---|---|
| Correctness | TOCTOU race | Fully atomic |
| Panic risk | `.unwrap()` panics on poison | Recovers via `into_inner()` |
| Throughput | Higher (RwLock parallel reads) | Slightly lower |
| Circuit breaker call frequency | Once per write (like/unlike) | Same — lock contention negligible |

The throughput trade-off is irrelevant: the circuit breaker is only called during content validation on `like`/`unlike` mutations, never on read paths.

---

## Dependencies to Add

```toml
dashmap = "6"
# tokio::sync::watch is already in tokio (full)
```

---

## Implementation Order

1. `circuit_breaker.rs` — single `Mutex` refactor (self-contained, existing tests verify correctness)
2. `like_service.rs` structural changes — trait object, `db_err` helper, deduplication
3. `like_service.rs` stampede fix — add `pending_fetches`, rewrite `get_count_inner`
4. `like_service.rs` skip validation on duplicates
5. `batch_counts` index fix
6. Run full test suite + integration tests
