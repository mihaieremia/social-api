# **Social API — Rust Microservice**

## **Context**

BeInCrypto operates a content platform serving 50M+ monthly pageviews across multiple content verticals. Each vertical is backed by its own API:

| Content Type | Service | Endpoint | Fields |
| ----- | ----- | ----- | ----- |
| `post` | Post API | `Mock — you implement` | `id(UUID), ...` |
| `bonus_hunter` | Bonus Hunter API | `Mock — you implement` | `id(UUID), ...` |
| `top_picks` | Top Picks API | `Mock — you implement` | `id(UUID), ...` |

User identity is managed by a **Profile API**. You can implement a mock. User id is also an UUID.

We are building a **Social API** — a new microservice responsible for social interactions across all content types, starting with **Likes**. This service must be production-ready, horizontally scalable, and designed to support additional social features (comments, bookmarks, shares) without architectural changes.

**Time limit:** 7 days.

---

## **Functional Requirements**

### **1\. Like / Unlike**

**Like:**

* Input: `content_type`, `content_id`, session token in `Authorization: Bearer <token>` header  
* Validate session token → extract `user_id`  
* Validate content exists by calling the corresponding Content API  
* Store the like with a timestamp. Duplicate requests are idempotent (return success, not error)  
* Update cached like count atomically

**Unlike:**

* Same validation chain  
* Remove the like record. Unliking content the user hasn't liked is idempotent (return success)  
* Update cached like count atomically

### **2\. Get Like Count (public)**

* Input: `content_type`, `content_id`  
* No authentication required  
* Must be served from cache. Fallback to database if cache is unavailable  
* **Target: p99 \< 5ms under sustained 10k rps**

### **3\. Get Like Status**

* Input: `content_type`, `content_id`, session token  
* Returns whether the authenticated user has liked this content, and the timestamp if so

### **4\. Get User's Liked Items**

* Input: session token, optional `content_type` filter, pagination params  
* Returns all content items the user has liked, ordered by most recent  
* **Must use cursor-based pagination** (not offset-based). Explain why in your code or README.

### **5\. Batch Get Like Counts**

* Input: array of `(content_type, content_id)` pairs (max 100\)  
* Returns like counts for all items in a single request  
* Called on every content listing page — this is the hottest path in the entire service

### **6\. Batch Get Like Statuses**

* Input: session token, array of `(content_type, content_id)` pairs (max 100\)  
* Returns like status for the authenticated user across all items  
* Called alongside batch counts on every listing page for authenticated users

### **7\. Get Top Liked Content**

* Input: `content_type` (optional), time window (`24h`, `7d`, `30d`, `all`), limit (max 50\)  
* Returns content items ranked by like count within the specified time window  
* **This is a leaderboard.** Design accordingly — naive implementations will not scale.

### **8\. Like Event Stream (SSE)**

* Endpoint: `GET /v1/likes/stream?content_type={type}&content_id={id}`  
* Server-Sent Events stream that pushes real-time like/unlike events for a specific content item  
* Used by the frontend to show live like count updates  
* Must handle backpressure and client disconnection gracefully

---

## **REST API**

POST   /v1/likes                                     — Like content  
DELETE /v1/likes/{content\_type}/{content\_id}         — Unlike content  
GET    /v1/likes/{content\_type}/{content\_id}/count   — Get like count (public)  
GET    /v1/likes/{content\_type}/{content\_id}/status  — Get like status (auth)  
GET    /v1/likes/user                                — Get user's likes (auth, paginated)  
POST   /v1/likes/batch/counts                        — Batch like counts (public)  
POST   /v1/likes/batch/statuses                      — Batch like statuses (auth)  
GET    /v1/likes/top                                 — Top liked content (public)  
GET    /v1/likes/stream                              — SSE like event stream (public)  
GET    /health/live                                  — Liveness probe  
GET    /health/ready                                 — Readiness probe  
GET    /metrics                                      — Prometheus metrics

### **Request/Response Contracts**

**POST /v1/likes**

// Request  
{  
  "content\_type": "post",  
  "content\_id": "731b0395-4888-4822-b516-05b4b7bf2089"  
}

// Response 201 Created  
{  
  "liked": true,  
  "already\_existed": false,  
  "count": 42,  
  "liked\_at": "2026-02-02T17:00:00Z"  
}

**DELETE /v1/likes/post/731b0395-4888-4822-b516-05b4b7bf2089**

// Response 200 OK  
{  
  "liked": false,  
  "was\_liked": true,  
  "count": 41  
}

**GET /v1/likes/post/731b0395-4888-4822-b516-05b4b7bf2089/count**

// Response 200 OK  
{  
  "content\_type": "post",  
  "content\_id": "731b0395-4888-4822-b516-05b4b7bf2089",  
  "count": 41  
}

**GET /v1/likes/post/731b0395-4888-4822-b516-05b4b7bf2089/status**

// Response 200 OK  
{  
  "liked": true,  
  "liked\_at": "2026-02-02T17:00:00Z"  
}

**GET /v1/likes/user?content\_type=post\&cursor=eyJ0IjoiMjAyNi...\&limit=20**

// Response 200 OK  
{  
  "items": \[  
    {  
      "content\_type": "post",  
      "content\_id": "731b0395-4888-4822-b516-05b4b7bf2089",  
      "liked\_at": "2026-02-02T17:00:00Z"  
    }  
  \],  
  "next\_cursor": "eyJ0IjoiMjAyNi0wMi0wMlQxNjowMDowMFoiLCJpZCI6IjEyMyJ9",  
  "has\_more": true  
}

**POST /v1/likes/batch/counts**

// Request  
{  
  "items": \[  
    { "content\_type": "post", "content\_id": "731b0395-4888-4822-b516-05b4b7bf2089" },  
    { "content\_type": "bonus\_hunter", "content\_id": "c3d4e5f6-a7b8-9012-cdef-123456789012" }  
  \]  
}

// Response 200 OK  
{  
  "results": \[  
    { "content\_type": "post", "content\_id": "731b0395-...", "count": 41 },  
    { "content\_type": "bonus\_hunter", "content\_id": "c3d4e5f6-...", "count": 7 }  
  \]  
}

**POST /v1/likes/batch/statuses**

// Request  
{  
  "items": \[  
    { "content\_type": "post", "content\_id": "731b0395-4888-4822-b516-05b4b7bf2089" },  
    { "content\_type": "bonus\_hunter", "content\_id": "c3d4e5f6-a7b8-9012-cdef-123456789012" }  
  \]  
}

// Response 200 OK  
{  
  "results": \[  
    { "content\_type": "post", "content\_id": "731b0395-...", "liked": true, "liked\_at": "2026-02-02T17:00:00Z" },  
    { "content\_type": "bonus\_hunter", "content\_id": "c3d4e5f6-...", "liked": false, "liked\_at": null }  
  \]  
}

**GET /v1/likes/top?content\_type=post\&window=7d\&limit=10**

// Response 200 OK  
{  
  "window": "7d",  
  "content\_type": "post",  
  "items": \[  
    { "content\_type": "post", "content\_id": "731b0395-...", "count": 1547 },  
    { "content\_type": "post", "content\_id": "9601c044-...", "count": 892 }  
  \]  
}

**GET /v1/likes/stream?content\_type=post\&content\_id=731b0395-4888-4822-b516-05b4b7bf2089**

// SSE stream  
data: {"event":"like","user\_id":"usr\_550e8400-...","count":42,"timestamp":"2026-02-02T17:00:01Z"}

data: {"event":"unlike","user\_id":"usr\_550e8400-...","count":41,"timestamp":"2026-02-02T17:00:05Z"}

data: {"event":"heartbeat","timestamp":"2026-02-02T17:00:15Z"}

### **Error Responses**

All errors follow a consistent structure:

{  
  "error": {  
    "code": "CONTENT\_NOT\_FOUND",  
    "message": "Content item does not exist or has been removed",  
    "request\_id": "req\_a1b2c3d4",  
    "details": {  
      "content\_type": "post",  
      "content\_id": "nonexistent-id"  
    }  
  }  
}

| Code | HTTP | Description |
| ----- | :---: | ----- |
| `UNAUTHORIZED` | 401 | Missing, malformed, or invalid token |
| `CONTENT_NOT_FOUND` | 404 | Content validation failed against Content API |
| `CONTENT_TYPE_UNKNOWN` | 400 | `content_type` not in configured registry |
| `INVALID_CONTENT_ID` | 400 | `content_id` is not a valid UUID v4 |
| `BATCH_TOO_LARGE` | 400 | Batch exceeds 100 items |
| `INVALID_CURSOR` | 400 | Pagination cursor is malformed or expired |
| `INVALID_WINDOW` | 400 | Time window not in `[24h, 7d, 30d, all]` |
| `RATE_LIMITED` | 429 | Rate limit exceeded. Include `Retry-After` header |
| `DEPENDENCY_UNAVAILABLE` | 503 | External service unreachable (after circuit breaker opens) |
| `INTERNAL_ERROR` | 500 | Unexpected error |

---

## **Technical Requirements**

### **Stack**

* **Language:** Rust (latest stable)  
* **Database:** PostgreSQL 15 (Read replica \+ Writer on real deployment)  
* **Cache:** Redis 7+  
* **Containerization:** Docker \+ Docker Compose

### **Database Design**

You design the schema. Requirements:

* One like per user per content item (enforced at DB level)  
* Efficient queries by content item (for counts) AND by user (for user's likes)  
* Like counts must be queryable by time window (last 24h, 7d, 30d) for the leaderboard  
* Schema applied via **versioned migrations** (not raw SQL in application code)  
* Consider: separate `like_counts` materialized table vs. computed aggregates vs. something else. Justify your choice.  
* Consider: indexing strategy. Every index you create has a write cost. Every missing index has a read cost. Be intentional.

### **Caching**

Implement a multi-layer caching strategy with Redis:

* Like counts (hot path — most critical)  
* Content validation results (external API responses)  
* User like statuses (evaluate whether this is worth caching)  
* Rate limit counters

You must handle:

* **Cache invalidation** on write operations  
* **Cache stampede / thundering herd** on popular content  
* **Redis unavailability** — the service must continue operating with degraded performance  
* **Cache warming** — how does the cache get populated after a cold start or Redis restart?  
* **Consistency** — what is the maximum staleness window? Document your decision.

### **Rate Limiting**

Per-user rate limiting on write operations (like/unlike):

* **30 requests per minute per user** on write endpoints  
* **1000 requests per minute per IP** on public read endpoints  
* Include `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset` headers on all responses  
* Include `Retry-After` header on 429 responses  
* Strategy must work correctly with multiple service replicas (shared state in Redis)

### **Circuit Breaker**

Implement circuit breaker pattern for all external service calls:

* **Closed → Open:** After 5 consecutive failures or \>50% failure rate in a 30-second window  
* **Open → Half-Open:** After 30 seconds  
* **Half-Open → Closed:** After 3 consecutive successes  
* When circuit is open:  
  * Content validation calls fail fast with `DEPENDENCY_UNAVAILABLE`  
  * **But:** read operations (get count, batch counts) must continue serving from cache/database  
  * Log circuit state transitions at `WARN` level

### **Graceful Shutdown**

The service must handle `SIGTERM`:

1. Stop accepting new connections  
2. Drain in-flight requests (30-second timeout)  
3. Close SSE connections with a final `shutdown` event  
4. Flush pending metrics  
5. Close database and Redis connection pools  
6. Exit with code 0

### **Observability**

**Structured logging** (JSON):

* Every log line must include: `timestamp`, `level`, `message`, `request_id`, `service`  
* Request logs: `method`, `path`, `status`, `latency_ms`, `user_id` (if authenticated)  
* Error logs: `error_type`, `error_message`, `stack_trace` (for 500s)  
* External call logs: `service`, `method`, `latency_ms`, `success`

**Metrics** (Prometheus, exposed at `/metrics`):

* `social_api_http_requests_total{method, path, status}` — counter  
* `social_api_http_request_duration_seconds{method, path}` — histogram  
* `social_api_cache_operations_total{operation, result}` — counter (hit/miss/error)  
* `social_api_external_calls_total{service, method, status}` — counter  
* `social_api_external_call_duration_seconds{service, method}` — histogram  
* `social_api_circuit_breaker_state{service}` — gauge (0=closed, 1=half-open, 2=open)  
* `social_api_db_pool_connections{state}` — gauge (active/idle/max)  
* `social_api_sse_connections_active` — gauge  
* `social_api_likes_total{content_type, operation}` — counter (like/unlike)

**Health checks:**

* `/health/live` — Returns 200 if process is running. No dependency checks. Used by Kubernetes liveness probe.  
* `/health/ready` — Returns 200 only if ALL of: database reachable, Redis reachable, at least one content API reachable. Returns 503 with details of what's failing. Used by Kubernetes readiness probe.

### **Configuration**

All configuration via environment variables. The service must fail fast on startup if required variables are missing.

\# Required  
READ\_DATABASE\_URL=postgres://social\_read\_replica:social\_password@postgres:5432/social\_api  
DATABASE\_URL=postgres://social:social\_password@postgres:5432/social\_api  
REDIS\_URL=redis://redis:6379  
HTTP\_PORT=8080

\# Content APIs (if only one service with multiple endpoints is used, adjust accordingly)  
CONTENT\_API\_POST\_URL=http://mock-post-api:8081  
CONTENT\_API\_BONUS\_HUNTER\_URL=http://mock-bonus-hunter-api:8082  
CONTENT\_API\_TOP\_PICKS\_URL=[http://mock-top-picks-api:8083](http://mock-top-picks-api:8083)  
\# Profile API  
PROFILE\_API\_URL=http://mock-profile-api:8084

\# Optional (with defaults)  
LOG\_LEVEL=info                          \# trace, debug, info, warn, error  
RUST\_LOG=social\_api=debug  
DB\_MAX\_CONNECTIONS=20  
DB\_MIN\_CONNECTIONS=5  
DB\_ACQUIRE\_TIMEOUT\_SECS=5  
REDIS\_POOL\_SIZE=10  
RATE\_LIMIT\_WRITE\_PER\_MINUTE=30  
RATE\_LIMIT\_READ\_PER\_MINUTE=1000  
CACHE\_TTL\_LIKE\_COUNTS\_SECS=300  
CACHE\_TTL\_CONTENT\_VALIDATION\_SECS=3600  
CACHE\_TTL\_USER\_STATUS\_SECS=60  
CIRCUIT\_BREAKER\_FAILURE\_THRESHOLD=5  
CIRCUIT\_BREAKER\_RECOVERY\_TIMEOUT\_SECS=30  
CIRCUIT\_BREAKER\_SUCCESS\_THRESHOLD=3  
SHUTDOWN\_TIMEOUT\_SECS=30  
SSE\_HEARTBEAT\_INTERVAL\_SECS=15  
LEADERBOARD\_REFRESH\_INTERVAL\_SECS=60

---

## **External Service Integration**

### **Content API (mock)**

You implement these (we have one service for each content\_type, but for simplicity you can use one service for all content\_types). Your mocks must:

* Expose: `GET /v1/{content_type}/{content_id}`  
* Return `200` with `{ "id": "...", "title": "...", "content_type": "..." }` for known IDs  
* Return `404` for unknown IDs  
* Pre-seed at least 5 valid UUIDs per content type  
* Behave consistently with the Post API's validation semantics  
* The content types are:  
  * `post`  
  * `bonus_hunter`  
  * `top_picks`

### **Profile API (mock)**

You implement this. Required interface:

**Validate token:**

GET /v1/auth/validate  
Authorization: Bearer \<token\>

Valid token response (`200`):

{  
  "valid": true,  
  "user\_id": "usr\_550e8400-e29b-41d4-a716-446655440001",  
  "display\_name": "Test User 1"  
}

Invalid token response (`401`):

{  
  "valid": false,  
  "error": "invalid\_token"  
}

**Tokens:**

| Token | User ID | Display Name |
| ----- | ----- | ----- |
| `tok_user_1` | `usr_550e8400-e29b-41d4-a716-446655440001` | Test User 1 |
| `tok_user_2` | `usr_550e8400-e29b-41d4-a716-446655440002` | Test User 2 |
| `tok_user_3` | `usr_550e8400-e29b-41d4-a716-446655440003` | Test User 3 |
| `tok_user_4` | `usr_550e8400-e29b-41d4-a716-446655440004` | Test User 4 |
| `tok_user_5` | `usr_550e8400-e29b-41d4-a716-446655440005` | Test User 5 |

### **Post API (live example)**

**Base URL**: https://beincrypto.com/api/post/v1

**Validate a post exists:**

curl \-s https://beincrypto.com/api/post/v1/post/731b0395-4888-4822-b516-05b4b7bf2089 | jq '.items\[0\].id'

Response shape:

{  
  "items": \[  
    {  
      "id": "731b0395-4888-4822-b516-05b4b7bf2089",  
      "title": "3 Altcoins Facing Major Liquidation Risks...",  
      "slug": "altcoins-facing-liquidation-in-1st-week-of-feb",  
      "status": "publish",  
      "author": { "id": "...", "name": "Nhat Hoang", "slug": "tung-nobi" },  
      "category": { "name": "Markets", "slug": "markets" },  
      "date": "2026-02-02T17:00:00Z",  
      "tags": \[{ "name": "Altcoin news", "slug": "altcoin-news" }\],  
      "content": "\<p\>...\</p\>",  
      "word\_count": 738  
    }  
  \],  
  "pagination": { "total": 1 }  
}

A post is valid if the response contains a non-empty items array. A non-existent ID returns 500 Error (this will be updated to 404 Not Found).

**List posts:**

curl \-s "https://beincrypto.com/api/post/v1/posts/?locale=en-US" | jq '.items | length'

**Test IDs:**

\- 731b0395-4888-4822-b516-05b4b7bf2089  
\- 9601c044-6130-4ee5-a155-96570e05a02f  
\- 933dde0f-4744-4a66-9a38-bf5cb1f67553  
\- ea0f2020-0509-45fd-adb9-24b8843055ee  
\- bd27f926-0a00-41fd-b085-a7491e6d0902  
\- 2a656157-5284-48b5-9d76-ede492933347  
\- 4f884e5e-2f1d-4965-b0f1-16922acd91a2  
\- ad1d9238-622c-4875-9881-5f8e19997783  
\- c34ee1e3-7224-4a97-ba44-0993eb7a6ed8  
\- c2b7f212-6162-4ae6-837b-16ee34cc9a50

### **Design Constraint**

Your code must abstract content validation behind a uniform interface. Adding a new content type (e.g., `news_article`) should require only configuration changes — zero code modifications.

In production, all inter-service communication will migrate to **gRPC/Protobuf**. Your architecture must make this transport swap possible without rewriting business logic. If you want to demonstrate this, implement the gRPC interface as a bonus.

---

## **Docker**

### **Dockerfile**

Multi-stage build. Final image must:

* Be based on `debian:bookworm-slim` or `distroless`  
* Not contain the Rust toolchain, source code, or build artifacts  
* Run as non-root user  
* Include health check instruction

### **docker-compose.yml**

Must start the entire system with `docker compose up --build`:

services:  
  social-api:             \# Your service  
  postgres:              \# PostgreSQL 16  
  redis:                    \# Redis 7+  
  mock-profile-api:  \# Your mock  
  mock-post-api:     \# Your mock (all three content APIs can be implemented in just one mock)  
  mock-bonus-hunter-api:  \# Your mock  
  mock-top-picks-api:         \# Your mock

Use `depends_on` with health checks. The social-api must not start until Postgres and Redis are healthy.

---

## **Testing**

### **Unit Tests**

* Like/unlike business logic and idempotency  
* Cache invalidation logic  
* Rate limiter behavior (window boundaries, counter reset)  
* Cursor encoding/decoding  
* Content type registry and validation  
* Circuit breaker state machine transitions  
* Error type mapping (domain → HTTP)

### **Integration Tests**

* Full like lifecycle: auth → validate content → store → cache → read  
* Batch operations with mix of valid/invalid items  
* Rate limiting with realistic request patterns  
* Circuit breaker triggering and recovery  
* Redis failure fallback (stop Redis container during test)  
* SSE connection lifecycle (connect, receive events, disconnect)  
* Pagination: forward cursor traversal, edge cases (empty results, last page)  
* Concurrent likes on same content (race condition verification)

### **Load Testing (optional)**

Provide a `k6` script that validates:

* **Read path:** 10,000 rps on `GET /v1/likes/{type}/{id}/count` with p99 \< 10ms  
* **Batch path:** 1,000 rps on `POST /v1/likes/batch/counts` (50 items per batch) with p99 \< 50ms  
* **Write path:** 500 rps on `POST /v1/likes` with p99 \< 100ms  
* **Mixed:** Realistic ratio of 80% reads / 15% batch / 5% writes

---

## **Deliverables**

1. Git repository with **meaningful commit history**  
2. `README.md` documenting architecture decisions, trade-offs, and how to run  
3. `Dockerfile` (multi-stage)  
4. `docker-compose.yml`  
5. Database migrations  
6. Mock service implementations  
7. Unit and integration tests  
8. OpenAPI spec (bonus)  
9. gRPC proto definitions \+ implementation for API-to-API comm (bonus)  
10. k6 load test script (bonus)

### **Run**

docker compose up \--build  
\# Service available at http://localhost:8080

\# Quick smoke test  
curl \-s http://localhost:8080/health/ready | jq .  
curl \-s http://localhost:8080/v1/likes/post/731b0395-4888-4822-b516-05b4b7bf2089/count | jq .  
curl \-X POST http://localhost:8080/v1/likes \\  
  \-H "Authorization: Bearer tok\_user\_1" \\  
  \-H "Content-Type: application/json" \\  
  \-d '{"content\_type":"post","content\_id":"731b0395-4888-4822-b516-05b4b7bf2089"}' | jq .

---

## **Evaluation**

| Area | Weight |
| ----- | :---: |
| Architecture & Design | 25% |
| Rust Proficiency | 25% |
| Data Design | 15% |
| Operational Readiness | 15% |
| Testing | 10% |
| Code Quality | 10% |

---

## **Notes**

* You may use AI tools.  
* Deliverable must be under the **MIT License**.  
* If you cannot complete everything in 7 days, prioritize: core like/unlike → caching → batch operations → everything else. Document what you'd do differently with more time.  
* Questions: email dani@beincrypto.com. We respond within 24h.

