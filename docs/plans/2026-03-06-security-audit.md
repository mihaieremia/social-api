# Security Audit Report: Social API Rust Microservice

**Date:** 2026-03-06
**Scope:** Full end-to-end audit of `crates/` (social-api, shared, mock-services)
**Methodology:** 4 parallel audit agents covering Auth/Token, Cache/Redis, SQL/Data, Input/API Surface

---

## Executive Summary

The Social API demonstrates strong foundational security: parameterized SQL via SQLx, structured error handling, graceful cache degradation, and circuit breaker patterns. However, the audit uncovered **8 CRITICAL**, **9 HIGH**, **8 MEDIUM**, and **5 LOW** findings across authentication, caching, data integrity, and API surface attack vectors.

The most urgent issues are the rate-limit bypass via Authorization header manipulation and the lack of request body size limits, both of which are exploitable today with zero prerequisites.

---

## CRITICAL FINDINGS

### C1. Rate Limit Bypass via Authorization Header Manipulation
**Severity:** CRITICAL
**Files:** `middleware/rate_limit.rs:183-190`
**Category:** Authentication Bypass

The write rate-limit middleware hashes the raw `Authorization` header value (including `Bearer ` prefix) to derive the rate-limit key. Three distinct attack vectors:

1. **Whitespace injection:** `"Bearer tok_user_1"` and `" Bearer tok_user_1"` produce different FNV-1a hashes, creating separate rate-limit buckets. An attacker can bypass the 30 req/min write limit by varying whitespace.

2. **Missing prefix stripping:** The hash includes `"Bearer "`, so the rate-limit identity differs from the actual authenticated user identity.

3. **Unauthenticated bucket poisoning:** All requests without an Authorization header hash to `fnv1a("")` -- a single shared bucket. An attacker can exhaust this bucket, and since rate limiting runs BEFORE auth extraction, malformed requests consume quota.

**Exploit:**
```bash
# Bypass: vary whitespace to get unlimited rate limit buckets
for i in $(seq 1 100); do
  spaces=$(printf '%*s' $i '')
  curl -H "Authorization:${spaces}Bearer tok_user_1" \
    -X POST http://api:8080/v1/likes -d '{"content_type":"post","content_id":"..."}'
done
```

**Fix:** Extract and normalize the token (strip `Bearer `, trim whitespace) before hashing. Reject requests with missing/malformed Authorization headers before rate limiting.

---

### C2. No Request Body Size Limit
**Severity:** CRITICAL
**Files:** `server.rs` (router setup, lines 110-188)
**Category:** Denial of Service

The Axum router does not configure `DefaultBodyLimit`. Axum 0.8's default is ~2GB. An attacker can send arbitrarily large JSON payloads to any POST endpoint, causing memory exhaustion before deserialization even completes.

**Exploit:**
```bash
# Send 500MB payload to batch endpoint
dd if=/dev/urandom bs=1M count=500 | curl -X POST http://api:8080/v1/likes/batch/counts \
  -H "Content-Type: application/json" --data-binary @-
```

**Fix:** Add `DefaultBodyLimit::max(1_048_576)` (1MB) as a global layer. For the like endpoint, 10KB is sufficient.

---

### C3. Unbounded SSE Connection Exhaustion
**Severity:** CRITICAL
**Files:** `handlers/stream.rs:42-65`
**Category:** Denial of Service

The `/v1/likes/stream` SSE endpoint has no per-IP or global connection limit. Each SSE connection holds a Tokio task, a broadcast receiver, and memory. An attacker can open thousands of connections, exhausting server resources.

**Exploit:**
```python
import asyncio, aiohttp
async def exhaust():
    async with aiohttp.ClientSession() as s:
        await asyncio.gather(*[
            s.get("http://api:8080/v1/likes/stream?content_type=post&content_id=731b0395-4888-4822-b516-05b4b7bf2089")
            for _ in range(10000)
        ])
asyncio.run(exhaust())
```

**Fix:**
- Per-IP SSE connection limit (e.g., max 10 concurrent)
- Global SSE connection cap (e.g., 5000)
- Connection coalescing for same `(content_type, content_id)` pair

---

### C4. SSE Slow-Read Attack (Backpressure/Memory Buildup)
**Severity:** CRITICAL
**Files:** `handlers/stream.rs:70-145`
**Category:** Denial of Service

SSE streams have no write timeout. A slow-reading client causes the broadcast channel buffer to fill. With `sse_broadcast_capacity = 128`, messages are dropped for ALL subscribers on that channel when one client lags.

**Fix:**
- Add write timeout (10s) per SSE client
- Drop clients that don't consume heartbeats within timeout
- Use `tokio::time::timeout` around each SSE frame write

---

### C5. Cursor Tampering / Cross-User Data Access
**Severity:** CRITICAL
**Files:** `shared/src/cursor.rs:36-44`, `services/like_service.rs:305-311`
**Category:** Authorization Bypass

Pagination cursors use unsigned base64url JSON `{"t":"...","id":123}` with no HMAC or user binding. An attacker can craft cursors to:
- Access other users' like history by manipulating the timestamp/id
- Enumerate the entire likes database by iterating through time windows

**Exploit:**
```bash
# Craft cursor to start from beginning of time
CURSOR=$(echo -n '{"t":"2020-01-01T00:00:00Z","id":0}' | base64)
curl "http://api:8080/v1/likes/user?cursor=$CURSOR&limit=100" \
  -H "Authorization: Bearer tok_user_1"
```

**Fix:**
- HMAC-SHA256 sign cursors with a server secret
- Bind cursor to `user_id` (include in signed payload)
- Add `issued_at` with 5-minute TTL
- Validate signature + user match on decode

---

### C6. Race Condition in Like/Unlike with Cache-DB Desync
**Severity:** CRITICAL
**Files:** `repositories/like_repository.rs:76-133`, `services/like_service.rs:114-122`
**Category:** Data Integrity

Two related issues form a compound vulnerability:

1. **Transaction isolation:** `insert_like()` and `delete_like()` use PostgreSQL's default `READ COMMITTED` isolation. Concurrent like/unlike on the same content can cause lost count updates.

2. **Cache-DB sync gap:** Cache is updated AFTER the DB transaction commits. If `cache.set()` fails silently (Redis down), the cache retains stale data. Subsequent reads via `conditional_incr()` can double-count.

**Scenario:**
```
T0: User A likes post (count: 4 -> 5). DB commits. cache.set("5") fails (Redis slow).
T1: User B likes post (count: 5 -> 6). DB commits. conditional_incr sees cache="4" (stale).
T2: conditional_incr increments "4" to "5" instead of "6". Count permanently desynchronized.
```

**Fix:**
- Use `SERIALIZABLE` isolation for like/unlike transactions
- OR: Use a single atomic Lua script for cache count updates that compares-and-sets
- Add periodic cache-DB reconciliation job

---

### C7. Silent Rollback Failure on Content Validation
**Severity:** CRITICAL (downgraded from original -- depends on content validation order)
**Files:** `services/like_service.rs:96-108`
**Category:** Data Integrity

When content validation fails AFTER `insert_like()` has committed, the rollback `delete_like()` uses `let _` (error swallowed). If the delete fails, an orphaned like persists in the database for content that doesn't exist.

**Fix:** Validate content BEFORE inserting the like, not after. This eliminates the rollback path entirely.

---

### C8. Circuit Breaker Abuse via Timeout Forcing
**Severity:** CRITICAL
**Files:** `extractors/auth.rs:40-64`
**Category:** Denial of Service

An attacker can force the Profile API circuit breaker open by sending requests with large tokens that cause HTTP timeouts. After 5 consecutive `DependencyUnavailable` errors, the circuit opens and ALL authenticated endpoints return 503 for 30 seconds.

**Exploit:**
```bash
# Send 5 requests with huge tokens that timeout
for i in {1..5}; do
  curl -H "Authorization: Bearer $(python3 -c 'print("A"*1000000)')" \
    http://api:8080/v1/likes/user
done
# All authenticated endpoints now return 503 for 30s
```

**Fix:**
- Validate token format (max length, character set) before sending to Profile API
- Rate limit by IP on auth-required endpoints (not just by token)
- Separate circuit breakers for auth validation vs content validation

---

## HIGH FINDINGS

### H1. Metrics Endpoint Publicly Exposed
**Files:** `server.rs:119-121`, `handlers/metrics_handler.rs:14`

`/metrics` exposes Prometheus data without authentication: connection pool stats, cache hit rates, SSE connection counts, external API call patterns. Useful for reconnaissance.

**Fix:** Serve metrics on a separate internal port (e.g., 9090) or add IP whitelist.

---

### H2. Missing Security Headers
**Files:** `server.rs` (router setup)

No `Strict-Transport-Security`, `X-Content-Type-Options: nosniff`, `X-Frame-Options`, or `Content-Security-Policy` headers.

**Fix:** Add `SetResponseHeaderLayer` for each header in the middleware stack.

---

### H3. Rate Limiting Fails Open on Redis Unavailability
**Files:** `middleware/rate_limit.rs:125-138`

When Redis is down, rate limiting is completely disabled (`allowed: true`). Combined with C1, an attacker who takes down Redis gets unlimited write access.

**Fix:** Implement local in-memory fallback rate limiter (per-process, `DashMap<String, AtomicU32>`) as backup.

---

### H4. Reader Pool Not Used for All Read Operations
**Files:** `repositories/like_repository.rs:106`

In `insert_like()`, when a row already exists, the existing like is read from the **writer pool** instead of the reader pool. Under high duplicate-like load, this starves the writer pool.

**Fix:** Use reader pool for the `SELECT` query in the "already exists" path.

---

### H5. Content-Type Header Not Validated
**Files:** `handlers/likes.rs:42-46`

Axum's `Json` extractor accepts `application/*+json` patterns loosely. Requests with `Content-Type: text/plain` containing valid JSON may be parsed.

**Fix:** Add explicit `Content-Type: application/json` validation in middleware.

---

### H6. Health Check Can Delay Graceful Shutdown
**Files:** `handlers/health.rs:46-112`, `main.rs:103-121`

`/health/ready` makes external API calls with 2s timeout. During graceful shutdown, flooding this endpoint delays the drain phase. Health checks bypass the inflight counter.

**Fix:** Include health checks in inflight counter. After shutdown signal, return 503 immediately without external calls.

---

### H7. Integer Overflow in Like Counts
**Files:** `migrations/002_create_like_counts.sql`, `repositories/like_repository.rs:170-178`

`like_counts.total_count` is BIGINT with `CHECK (total_count >= 0)` but no upper bound. PostgreSQL wraps at `i64::MAX + 1`. Redis INCRBY has the same issue.

**Fix:** Add `CHECK (total_count < 9223372036854775800)` to the schema. Add bounds check in the Lua INCR script.

---

### H8. Batch Size Validated After Deserialization
**Files:** `handlers/likes.rs:189-205`, `services/like_service.rs:356-360`

An attacker can send a 100MB JSON array with millions of items. Serde deserializes the entire payload into memory before the `items.len() > 100` check runs.

**Fix:** Check `Content-Length` header in middleware. Add custom deserializer that aborts after 100 items.

---

### H9. Timing Attack on Mock Token Comparison
**Files:** `mock-services/src/profile.rs:21-32`

Token comparison uses `==` (non-constant-time). Allows token enumeration via response time correlation.

**Fix:** Use `subtle::ConstantTimeEq` for token comparison. (Mock only, but sets a bad pattern.)

---

## MEDIUM FINDINGS

### M1. Auth Error Messages Leak Implementation Details
**Files:** `extractors/auth.rs:88-109`

Different error messages for missing header, malformed header, invalid token, and service unavailable allow attackers to fingerprint the auth flow.

**Fix:** Return generic `401 Unauthorized` for all auth failures. Log specifics server-side.

---

### M2. Leaderboard Refresh Creates Empty Window
**Files:** `cache/manager.rs:365-393`

`replace_sorted_set()` does `DEL` then `ZADD` atomically in Redis, but concurrent reads between refresh cycles see empty results for ~100-500ms.

**Fix:** Use a RENAME-based swap: write to temp key, then `RENAME tempkey realkey` (atomic).

---

### M3. FNV-1a Hash Collision Risk for Token Cache Keys
**Files:** `clients/profile_client.rs:35-46`, `middleware/rate_limit.rs:255-264`

FNV-1a is non-cryptographic. Two tokens hashing to the same 64-bit value would share a rate-limit bucket or cache entry. Probability is low but nonzero.

**Fix:** Use SipHash (Rust's default `HashMap` hasher) or SHA-256 truncated to 128 bits.

---

### M4. Empty Batch Requests Accepted
**Files:** `handlers/likes.rs:189-205`

`{"items": []}` passes validation. Wastes server resources with no useful output.

**Fix:** Add `if items.is_empty() { return Err(AppError::batch_empty()) }`.

---

### M5. Cursor Timestamp Precision Loss
**Files:** `shared/src/cursor.rs:17-18`, `repositories/like_repository.rs:273`

Cursor encodes `DateTime<Utc>` which may lose microsecond precision during JSON serialization. Under high throughput, rows created in the same millisecond may be duplicated or skipped during pagination.

**Fix:** Store cursor timestamp as microseconds-since-epoch (i64).

---

### M6. SSE Channel Name From User Input
**Files:** `handlers/stream.rs:56`, `services/like_service.rs:131`

Redis pub/sub channel constructed via `format!("sse:{content_type}:{content_id}")`. Content type is validated against the registry (mitigated), but if a new type with special characters (`:`, `*`) is added, it could cause namespace pollution.

**Fix:** Add character-class validation to content type registry (alphanumeric + underscore only).

---

### M7. Graceful Degradation Hides Persistent Redis Failures
**Files:** `cache/manager.rs` (all methods)

All Redis operations silently return `None` on failure with only a `warn!` log. If Redis is down for extended periods, no alerting fires unless someone monitors `cache_operations_total{result="error"}`.

**Fix:** Add a circuit breaker or error-rate counter that triggers alerts when cache error rate exceeds 10%.

---

### M8. No Connection Pool Validation on Startup
**Files:** `db.rs:18-53`

`DbPools::from_config()` creates pools without verifying that `min_connections` can be acquired. Wrong credentials or unreachable DB silently fails until first request.

**Fix:** Add `pool.acquire().await?` in `from_config()` as a connectivity check.

---

## LOW FINDINGS

### L1. Token Cache FNV-1a Collision (Theoretical Session Hijack)
**Files:** `clients/profile_client.rs:35-46`
Two tokens colliding would cause one user to receive another's cached auth result. Probability: ~2^(-64).

### L2. Request ID Not Globally Unique
**Files:** `middleware/request_id.rs:14-21`
Format `req_{hex_ts}-{hex_seq}` resets on restart and is predictable. Fine for tracing, not for security tokens.

### L3. Non-Numeric Cache Corruption Not Recovered
**Files:** `cache/manager.rs:26-29`
If a cache key contains a non-numeric string, `INCRBY` errors. The key stays corrupted until TTL expires.

### L4. Pending Fetches DashMap Leak (Theoretical)
**Files:** `services/like_service.rs:238-269`
If the fetcher task is cancelled between `insert()` and `send()`, the DashMap entry persists. Waiters receive `Err` and fall through to DB (safe but wasteful).

### L5. Mock Profile Service Has No Rate Limiting
**Files:** `mock-services/src/profile.rs:12-43`
Not production-relevant but sets bad testing patterns.

---

## Remediation Priority Matrix

### Phase 1: Deploy Blockers (Fix Before Production)

| ID | Finding | Effort |
|----|---------|--------|
| C1 | Rate limit key normalization | 2h |
| C2 | Add `DefaultBodyLimit` | 30min |
| C5 | HMAC-sign cursors | 4h |
| C7 | Validate content BEFORE insert | 2h |
| C8 | Token format validation before Profile API call | 1h |
| H1 | Move `/metrics` to internal port | 1h |
| H3 | Local fallback rate limiter | 4h |

### Phase 2: High Priority (Next Sprint)

| ID | Finding | Effort |
|----|---------|--------|
| C3 | SSE per-IP connection limit | 4h |
| C4 | SSE write timeout | 3h |
| C6 | Serializable isolation + cache reconciliation | 8h |
| H2 | Security headers | 1h |
| H4 | Reader pool for read queries | 30min |
| H8 | Pre-deserialization size check | 3h |
| M1 | Generic auth errors | 1h |

### Phase 3: Hardening (Backlog)

| ID | Finding | Effort |
|----|---------|--------|
| H5 | Content-Type validation | 1h |
| H6 | Health check inflight tracking | 2h |
| H7 | BIGINT overflow protection | 1h |
| M2 | Leaderboard RENAME swap | 2h |
| M3 | Upgrade to SipHash | 1h |
| M4-M8 | Remaining medium findings | 4h |

---

## What's Done Well

- **SQL injection:** Zero risk. All queries use SQLx parameterized bindings.
- **Error model:** Consistent JSON envelope with error codes. No stack traces leaked.
- **Cache degradation:** Redis failures never propagate panics; service continues on DB.
- **Circuit breaker:** Proper state machine (Closed/HalfOpen/Open) with configurable thresholds.
- **Content type registry:** Config-driven, prevents arbitrary content types in most paths.
- **Graceful shutdown:** Proper SIGTERM handling with drain, SSE close, and connection pool cleanup.
- **Structured logging:** JSON format with request_id correlation throughout.
- **Middleware ordering:** inflight -> request_id -> error_context -> metrics -> rate_limit -> handler is correct.
