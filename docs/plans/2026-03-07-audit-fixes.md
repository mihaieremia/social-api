# Audit Fixes Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix 3 high-severity behavior bugs and 5 medium/low documentation drift issues identified by static audit.

**Architecture:** Two independent streams — behavior fixes (sequential, overlapping files) and doc fixes (parallel, independent files). Behavior fixes change error classification in HTTP clients, add validation to unlike, and reorder like to validate-before-insert.

**Tech Stack:** Rust (axum, reqwest, sqlx), k6/JS, Makefile, Markdown

---

## Stream 1: Behavior Fixes (sequential)

### Task 1: Fix HTTP error classification in content_client.rs

**Files:**
- Modify: `crates/social-api/src/clients/content_client.rs:89-99`

**Step 1: Change content_client to distinguish 4xx from 5xx**

In `content_client.rs`, replace the current logic at lines 89-99 that treats all non-2xx as `valid = false`. Instead:
- 2xx → `Ok(true)` (valid content, cache 3600s)
- 404 → `Ok(false)` (content not found, cache 60s)
- Other 4xx → `Ok(false)` (treat as not found, cache 60s)
- 5xx → `Err(AppError::DependencyUnavailable)` (upstream failure, do NOT cache)

```rust
        let status = response.status();

        if status.is_success() {
            // Content exists — cache as valid for 3600s
            self.cache
                .set(&cache_key, "1", self.config.cache_ttl_content_validation_secs)
                .await;
            tracing::debug!(
                service = "content_api",
                content_type = content_type,
                content_id = %content_id,
                valid = true,
                "Content validation result"
            );
            Ok(true)
        } else if status.is_server_error() {
            // 5xx — upstream failure, propagate as dependency error (do NOT cache)
            tracing::error!(
                service = "content_api",
                content_type = content_type,
                content_id = %content_id,
                status = status.as_u16(),
                "Content API returned server error"
            );
            Err(AppError::DependencyUnavailable("content_api".to_string()))
        } else {
            // 4xx (including 404) — content not found, cache briefly to avoid hammering
            self.cache
                .set(&cache_key, "0", 60)
                .await;
            tracing::debug!(
                service = "content_api",
                content_type = content_type,
                content_id = %content_id,
                valid = false,
                "Content validation result"
            );
            Ok(false)
        }
```

**Step 2: Run tests**

Run: `cargo test --workspace --bins -- content_client`
Expected: existing tests pass (they don't test HTTP responses directly)

**Step 3: Commit**

```
fix: content_client distinguishes 5xx (DependencyUnavailable) from 4xx (not found)

Previously, any non-2xx response was classified as "content not found"
and cached for 60s. A 500/503 from the Content API would surface as
CONTENT_NOT_FOUND (404) and the circuit breaker would never see the
failure. Now 5xx responses propagate as DependencyUnavailable errors,
allowing the circuit breaker to trip correctly.
```

---

### Task 2: Fix HTTP error classification in profile_client.rs

**Files:**
- Modify: `crates/social-api/src/clients/profile_client.rs:91-95`

**Step 1: Change profile_client to distinguish 4xx from 5xx**

Replace lines 91-95 that treat all non-2xx as `Unauthorized`:

```rust
        if response.status().is_server_error() {
            tracing::error!(
                service = "profile_api",
                status = response.status().as_u16(),
                "Profile API returned server error"
            );
            return Err(AppError::DependencyUnavailable("profile_api".to_string()));
        }

        if !response.status().is_success() {
            return Err(AppError::Unauthorized(
                "Invalid or expired token".to_string(),
            ));
        }
```

This ensures 5xx from Profile API → `DependencyUnavailable` → `auth.rs` records breaker failure → breaker can trip. 4xx still → `Unauthorized` → 401.

**Step 2: Run tests**

Run: `cargo test --workspace --bins -- profile_client`
Expected: PASS (existing tests are pure hash tests, not HTTP)

**Step 3: Commit**

```
fix: profile_client returns DependencyUnavailable on 5xx instead of Unauthorized

Previously, any non-2xx from Profile API was mapped to Unauthorized
(401), so the circuit breaker in auth.rs never recorded failures for
upstream 500/503 errors. Now 5xx responses propagate as
DependencyUnavailable, allowing the breaker to trip and return 503.
```

---

### Task 3: Fix like race condition — validate before insert

**Files:**
- Modify: `crates/social-api/src/services/like_service.rs:76-138`

**Step 1: Reorder like() to validate content BEFORE inserting**

Replace the current like() method body. The key change: call `validate_content()` before `insert_like()`, so no row exists for a concurrent duplicate to observe while validation is pending.

```rust
    pub async fn like(
        &self,
        user_id: Uuid,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeActionResponse, AppError> {
        // Validate content exists BEFORE inserting.
        // This prevents a race where a concurrent duplicate request could observe
        // an unvalidated row and return success before this request rolls it back.
        self.validate_content(content_type, content_id).await?;

        // Now insert — content is known-valid. Duplicate detection is safe because
        // any existing row was validated on its original insert.
        let result =
            like_repository::insert_like(&self.db.writer, user_id, content_type, content_id)
                .await
                .map_err(Self::db_err)?;

        // Use the authoritative count from the write transaction to avoid
        // races where a concurrent request reads a stale count from the reader.
        let count = result.count;
        self.write_count_to_cache(content_type, content_id, count)
            .await;

        if !result.already_existed {
            self.publish_like_event(
                LikeEvent::Liked {
                    user_id,
                    count,
                    timestamp: result.row.created_at,
                },
                content_type,
                content_id,
            )
            .await;
            Self::record_like_metric("like", content_type);
        }

        Ok(LikeActionResponse {
            liked: true,
            already_existed: Some(result.already_existed),
            was_liked: None,
            count,
            liked_at: Some(result.row.created_at),
        })
    }
```

**Trade-off:** Duplicate likes now re-validate content on every request (one extra Content API call). This is acceptable because:
1. Content validation is cached (3600s TTL), so most duplicates hit cache
2. Correctness > performance for a mutation endpoint
3. The spec explicitly requires content validation on every like

**Step 2: Run unit tests**

Run: `cargo test --workspace --bins -- like_service`
Expected: PASS

**Step 3: Commit**

```
fix: validate content before insert in like() to close race condition

Previously, insert_like() ran before validate_content(), creating a
window where a concurrent duplicate could observe the unvalidated row
and return already_existed=true before the first request's validation
failure rolled it back. Now validation runs first, eliminating the race.
Duplicate likes may re-validate but content validation is cached (3600s).
```

---

### Task 4: Add content validation to unlike

**Files:**
- Modify: `crates/social-api/src/services/like_service.rs:143-181`

**Step 1: Add validate_content() call before delete_like()**

The spec requires unlike to use the "same validation chain" as like. Add content validation:

```rust
    pub async fn unlike(
        &self,
        user_id: Uuid,
        content_type: &str,
        content_id: Uuid,
    ) -> Result<LikeActionResponse, AppError> {
        // Validate content exists — spec requires same validation chain as like.
        self.validate_content(content_type, content_id).await?;

        let result =
            like_repository::delete_like(&self.db.writer, user_id, content_type, content_id)
                .await
                .map_err(Self::db_err)?;

        // Use the authoritative count from the write transaction — same fix as like().
        let count = result.count;
        self.write_count_to_cache(content_type, content_id, count)
            .await;

        // Publish SSE event
        if result.was_liked {
            self.publish_like_event(
                LikeEvent::Unliked {
                    user_id,
                    count,
                    timestamp: Utc::now(),
                },
                content_type,
                content_id,
            )
            .await;
            Self::record_like_metric("unlike", content_type);
        }

        Ok(LikeActionResponse {
            liked: false,
            already_existed: None,
            was_liked: Some(result.was_liked),
            count,
            liked_at: None,
        })
    }
```

**Step 2: Run unit tests**

Run: `cargo test --workspace --bins -- like_service`
Expected: PASS

**Step 3: Run full build**

Run: `cargo build`
Expected: compiles clean

**Step 4: Commit**

```
fix: add content validation to unlike, matching spec's same-validation-chain requirement

The spec requires unlike to use the same validation chain as like.
Previously, unlike skipped content existence validation and went
straight to delete_like(). Now it calls validate_content() first,
returning CONTENT_NOT_FOUND if the content no longer exists in the
Content API.
```

---

## Stream 2: Documentation & Config Fixes (parallel — all independent)

### Task 5: Fix k6 thresholds and docs

**Files:**
- Modify: `k6/load_test.js:322`
- Modify: `k6/README.md:7,49-54`
- Modify: `Makefile:125`

**Step 1: Fix k6 read_path threshold to match spec**

In `k6/load_test.js:322`, change:
```js
    'http_req_duration{scenario:read_path}': ['p(99)<5'],
```

**Step 2: Fix Makefile comment**

In `Makefile:125`, change:
```makefile
k6-load-read: ## Load: isolated read path (10k rps, 60s)
```

**Step 3: Fix k6/README.md**

- Line 7: Change "all 4 services" to "all services" (compose has 7+ services)
- Line 51: Change `< 10ms` to `< 5ms` in the threshold table

**Step 4: Commit**

```
docs: reconcile k6 thresholds with spec (p99 < 5ms for reads at 10k rps)

The spec requires p99 < 5ms. The k6 test enforced < 10ms, and the
Makefile comment said 8k rps instead of 10k. Fixed all three to match.
Also fixed k6/README service count reference.
```

---

### Task 6: Fix test Makefile targets

**Files:**
- Modify: `Makefile:89-90`
- Modify: `README.md:50-52`

**Step 1: Fix Makefile test targets to include library tests**

In `Makefile:89-90`, change `--bins` to `--lib --bins`:
```makefile
test-unit: ## Run unit tests — real infra via testcontainers (requires Docker)
	cargo test --workspace --lib --bins
```

In `Makefile:92-93`, same fix:
```makefile
test-unit-fast: ## Run only pure unit tests — no containers, no Docker needed
	cargo test --workspace --lib --bins \
```

**Step 2: Fix README descriptions**

In `README.md:50-52`:
```markdown
make test               # Pure unit tests (no containers) — no Docker needed
make test-unit          # All unit tests (lib + bin) — uses testcontainers (needs Docker daemon)
```

**Step 3: Commit**

```
fix: include library tests in make test-unit (--lib --bins instead of --bins)

The Makefile used --bins which excluded library test modules in
like_service, like_repository, and rate_limit. Added --lib flag
and updated README descriptions to match.
```

---

### Task 7: Fix .env.example and README config docs

**Files:**
- Modify: `.env.example`
- Modify: `README.md:80-85`

**Step 1: Add missing config vars to .env.example**

Append after line 33:
```env

# gRPC / Internal transport
GRPC_PORT=50051
INTERNAL_TRANSPORT=http
INTERNAL_GRPC_URL=http://localhost:50052

# Server tuning
SERVER_CONCURRENCY_LIMIT=10000
SSE_BROADCAST_CAPACITY=128

# Circuit breaker (advanced)
CIRCUIT_BREAKER_RATE_WINDOW_SECS=30
```

**Step 2: Fix README required config section**

In `README.md:80-85`, replace:
```markdown
Required:
```
DATABASE_URL, READ_DATABASE_URL, REDIS_URL, HTTP_PORT
PROFILE_API_URL
At least one CONTENT_API_{TYPE}_URL (e.g., CONTENT_API_POST_URL)
```
```

**Step 3: Commit**

```
docs: add missing config vars to .env.example and fix README requirements

Added GRPC_PORT, INTERNAL_TRANSPORT, INTERNAL_GRPC_URL,
SERVER_CONCURRENCY_LIMIT, SSE_BROADCAST_CAPACITY, and
CIRCUIT_BREAKER_RATE_WINDOW_SECS to .env.example.
Fixed README to show only PROFILE_API_URL + at least one
CONTENT_API_{TYPE}_URL are required (not all three).
```

---

### Task 8: Update CLAUDE.md

**Files:**
- Modify: `CLAUDE.md`

**Step 1: Fix transport comment**

Change line 148 area from "HTTP today, gRPC tomorrow" to reflect dual-transport:
```
// Transport-swappable (HTTP and gRPC, selected via INTERNAL_TRANSPORT env var)
```

**Step 2: Add gRPC clients to module inventory**

In the clients/ section, add:
```
  grpc_profile_client.rs         # GrpcTokenValidator (tonic)
  grpc_content_client.rs         # GrpcContentValidator (tonic)
```

**Step 3: Fix AppState description**

Update line 30 (state.rs description) to include `pubsub_manager`:
```
state.rs  # AppState (Arc<AppStateInner>: db, cache, config, http_client, like_service, token_validator, profile_breaker, pubsub_manager, shutdown_token, inflight_count)
```

**Step 4: Fix rate limiter key doc**

Change `fnv1a_hash(token)` to `fnv1a_hash(authorization_header)` in the Redis key table.

**Step 5: Commit**

```
docs: update CLAUDE.md to match current architecture

- Transport comment: "HTTP today, gRPC tomorrow" → dual-transport via INTERNAL_TRANSPORT
- Added gRPC clients (grpc_profile_client.rs, grpc_content_client.rs) to module inventory
- Added pubsub_manager to AppState description
- Fixed rate limiter key doc (hashes full authorization header, not just token)
```

---

### Task 9: Fix stale inline comments

**Files:**
- Modify: `crates/social-api/src/server.rs:24,140`
- Modify: `crates/social-api/src/grpc/health_service.rs:1-4`
- Modify: `crates/social-api/src/middleware/rate_limit.rs:204-206`
- Modify: `crates/social-api/src/middleware/request_id.rs:12`

**Step 1: Fix server.rs comments**

Line 24: Change `500ms` to `2000ms`:
```rust
/// - `errors`: log only 4xx/5xx or requests slower than 2000ms (production default).
```

Line 140: Change "Read routes (GET)" to reflect batch POSTs:
```rust
    // Read routes (GET + batch POST) — per-IP read rate limit
```

**Step 2: Fix gRPC health service doc comment**

Lines 1-4: Update to reflect actual behavior:
```rust
//! gRPC health check service implementation.
//!
//! Implements `social.v1.Health.Check` with database and Redis dependency
//! checks. Note: unlike HTTP `/health/ready`, this does NOT check content APIs.
```

**Step 3: Fix rate_limit.rs comment**

Lines 204-206: Remove false security claim:
```rust
/// Uses the RIGHTMOST value in X-Forwarded-For — the address appended by the
/// last trusted proxy (load balancer). Assumes deployment behind a trusted
/// reverse proxy that sets this header. Falls back to X-Real-IP, then "unknown".
```

**Step 4: Fix request_id.rs format doc**

Line 12: Fix separator character:
```rust
/// Format: `req_{hex_timestamp}-{hex_counter}` — sortable and collision-free
```

**Step 5: Commit**

```
docs: fix 6 stale inline comments across server, health, rate_limit, request_id

- server.rs: slow log threshold 500ms → 2000ms; "Read routes (GET)" → includes batch POST
- grpc/health_service.rs: note it does NOT check content APIs (unlike HTTP readiness)
- rate_limit.rs: remove "cannot forge" claim; note trusted-proxy assumption
- request_id.rs: fix format doc separator _ → -
```

---

## Execution Order

```
Stream 1 (sequential):  Task 1 → Task 2 → Task 3 → Task 4
Stream 2 (parallel):    Task 5 | Task 6 | Task 7 | Task 8 | Task 9

Both streams are independent — run simultaneously.
```

## Verification Checklist

After all tasks:
- [ ] `cargo build` compiles clean
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo test --workspace --lib --bins` passes
- [ ] `cargo test --workspace --test '*' -- --ignored` passes (integration)
- [ ] Review git log for 9 clean commits
