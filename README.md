# Social API -- Rust Microservice

A production-ready Social API microservice for BeInCrypto, handling **likes** across multiple content types (`post`, `bonus_hunter`, `top_picks`). Designed for horizontal scalability and 10k+ rps on read paths.

## Quick Start

```bash
docker compose up --build
# Service: http://localhost:8080

# Smoke test
curl -s http://localhost:8080/health/ready | jq .
curl -s http://localhost:8080/v1/likes/post/731b0395-4888-4822-b516-05b4b7bf2089/count | jq .
curl -X POST http://localhost:8080/v1/likes \
  -H "Authorization: Bearer tok_user_1" \
  -H "Content-Type: application/json" \
  -d '{"content_type":"post","content_id":"731b0395-4888-4822-b516-05b4b7bf2089"}' | jq .
```

## Architecture

### Workspace Layout

```
social-api/
├── crates/
│   ├── social-api/          # Main service binary
│   ├── mock-services/       # Single binary serving all mock APIs
│   └── shared/              # Common types, errors, cursor
├── migrations/              # SQLx migrations
├── docker-compose.yml
├── Dockerfile               # Multi-stage (social-api)
├── Dockerfile.mock          # Multi-stage (mock-services)
├── k6/                      # Load tests (k6)
├── monitoring/              # Monitoring configs
├── scripts/                 # Utility scripts
└── docs/                    # Design docs and plans
```

### Data Flow

```
Request -> inflight -> request_id -> error_context -> metrics -> rate_limit -> handler
  handler -> extractor (auth via profile_breaker + Profile API; content type registry check)
  handler -> service (business logic)
    service -> cache (Redis, graceful fallback to DB on miss/error)
    service -> repository (SQLx, writer pool for mutations, reader pool for reads)
    service -> content_breaker -> ContentValidator (HTTP Content API, cached)
  handler -> JSON response (error_context patches request_id into error bodies)
```

### Key Design Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Like counts | Separate `like_counts` table | O(1) reads, avoids COUNT(*) on hot path. Extra write cost acceptable for 80/20 read/write. |
| Pagination | Cursor-based (timestamp+id) | Index-seekable, stable under concurrent writes, no row skipping at depth. |
| Caching | Redis cache-aside + stampede lock | SET NX with 5s TTL on cache miss — only one fetcher hits DB, others wait 50ms then retry cache. Like count updates use conditional Lua scripts (INCR/DECR only if key exists) to prevent ghost counts. |
| SSE | Redis Pub/Sub | Multi-replica support. Each instance subscribes to relevant channels. |
| Content types | Config-driven registry | Adding new type = one env var. Zero code changes. |
| External clients | Trait-based abstraction | Transport-swappable (HTTP -> gRPC) without rewriting business logic. |

## API Reference

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| POST | `/v1/likes` | Yes | Like content |
| DELETE | `/v1/likes/{type}/{id}` | Yes | Unlike content |
| GET | `/v1/likes/{type}/{id}/count` | No | Get like count (cached) |
| GET | `/v1/likes/{type}/{id}/status` | Yes | Get user's like status |
| GET | `/v1/likes/user` | Yes | User's likes (cursor paginated) |
| POST | `/v1/likes/batch/counts` | No | Batch counts (max 100) |
| POST | `/v1/likes/batch/statuses` | Yes | Batch statuses (max 100) |
| GET | `/v1/likes/top` | No | Leaderboard (24h/7d/30d/all) |
| GET | `/v1/likes/stream` | No | SSE live events |
| GET | `/health/live` | No | Liveness probe |
| GET | `/health/ready` | No | Readiness probe |
| GET | `/metrics` | No | Prometheus metrics |
| GET | `/swagger-ui` | No | Swagger UI (OpenAPI spec browser) |
| GET | `/api-docs/openapi.json` | No | OpenAPI 3.0 JSON spec |

## Database Schema

Two tables: `likes` (events) and `like_counts` (materialized counters).

```sql
-- likes: one row per user-content interaction
-- Unique constraint enforces one like per user per content item
CREATE TABLE likes (
    id           BIGSERIAL PRIMARY KEY,
    user_id      UUID NOT NULL,
    content_type VARCHAR(50) NOT NULL,
    content_id   UUID NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT uq_likes_user_content UNIQUE (user_id, content_type, content_id)
);

-- like_counts: O(1) count reads, updated atomically in same transaction as likes
CREATE TABLE like_counts (
    content_type VARCHAR(50) NOT NULL,
    content_id   UUID NOT NULL,
    total_count  BIGINT NOT NULL DEFAULT 0,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (content_type, content_id),
    CONSTRAINT chk_total_count_non_negative CHECK (total_count >= 0)
);
```

### Indexing Strategy

| Index | Covers | Write Cost |
|-------|--------|------------|
| `idx_likes_user_created (user_id, created_at DESC, id DESC)` | Cursor pagination for user's likes | Low — append-only pattern |
| `idx_likes_content (content_type, content_id)` | Count fallback queries | Low |
| `idx_likes_created_ct_cid (created_at, content_type, content_id)` | Time-windowed leaderboard | Moderate — B-tree composite enables index range scan + GROUP BY |

## Redis Key Design

| Key | Type | TTL | Purpose |
|-----|------|-----|---------|
| `lc:{type}:{id}` | STRING | 300s | Like count cache |
| `cv:{type}:{id}` | STRING | 3600s/60s | Content validation cache |
| `rl:w:{hash}` | ZSET | 70s | Write rate limit (30/min) |
| `rl:r:{hash}` | ZSET | 70s | Read rate limit (1000/min) |
| `lb:{window}` | ZSET | no TTL | Leaderboard per window (score=count, member="{type}:{id}"); content_type filter in app |
| `sse:{type}:{id}` | PUB/SUB | - | SSE event channel |

### Cache Consistency

- **Max staleness:** 300s for counts, 3600s for content validation
- **Invalidation:** Conditional INCR/DECR via Lua scripts (only updates if key exists, preventing ghost counts)
- **Stampede protection:** SET NX lock with 5s TTL on count cache miss; losers wait 50ms then retry cache, then fallback to DB
- **Redis failure:** Graceful degradation — all reads fallback to DB, never error to client

## Resilience

### Circuit Breaker

For all external service calls (Content API, Profile API):
- **Closed -> Open:** 5 consecutive failures
- **Open -> Half-Open:** After 30s
- **Half-Open -> Closed:** 3 consecutive successes
- Read operations continue from cache/DB when circuit is open

### Rate Limiting

- **Write:** 30 requests/minute per user (Redis sliding window via Lua script)
- **Read:** 1000 requests/minute per IP
- Headers: `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset`
- `Retry-After` on 429 responses

## Observability

### Metrics (Prometheus at `/metrics`)

- `social_api_http_requests_total{method, path, status}`
- `social_api_http_request_duration_seconds{method, path}`
- `social_api_cache_operations_total{operation, result}`
- `social_api_external_calls_total{service, method, status}`
- `social_api_external_call_duration_seconds{service, method}`
- `social_api_circuit_breaker_state{service}`
- `social_api_sse_connections_active`
- `social_api_likes_total{content_type, operation}`
- `social_api_db_pool_connections{pool, state}`

### Structured Logging

JSON-formatted logs via `tracing`:
- Every line: `timestamp`, `level`, `message`, `target`
- Requests: `method`, `path`, `status`, `latency_ms`, `request_id`
- External calls: `service`, `method`, `latency_ms`, `success`

## Configuration

All via environment variables. See `.env.example` for full list.

Required:
```
DATABASE_URL=postgres://social:social_password@postgres:5432/social_api
READ_DATABASE_URL=postgres://social:social_password@postgres:5432/social_api
REDIS_URL=redis://redis:6379
HTTP_PORT=8080
PROFILE_API_URL=http://mock-services:8081
CONTENT_API_POST_URL=http://mock-services:8081
```

## Testing

```bash
# Unit tests (no infrastructure needed)
cargo test --workspace --bins

# HTTP tests (uses testcontainers, no manual setup)
cargo test --workspace --test http_test

# Integration tests (requires docker compose up)
docker compose up -d
cargo test --test integration_test -p social-api -- --ignored
cargo test --test graceful_shutdown_test -p social-api -- --ignored
```

**146 unit tests** covering:
- Like service business logic (23 tests)
- Config env-var parsing and validation (19 tests)
- Cache manager operations (14 tests)
- Error code mapping and API error serialization (14 tests)
- Circuit breaker state transitions, failure rate, half-open recovery (13 tests)
- Like repository SQL operations (12 tests)
- Rate limiting sliding window logic (8 tests)
- Metrics path normalization and registration (8 tests)
- TimeWindow parsing, duration, display, serialization (7 tests)
- PubSub manager channel subscriptions (6 tests)
- Leaderboard refresh task (5 tests)
- Cursor encode/decode roundtrip and invalid input (4 tests)
- Shutdown drain and signal handling (4 tests)
- DB pool metrics emission (4 tests)
- AppState construction (3 tests)
- Profile client token validation (2 tests)

**67 integration tests** covering:
- HTTP endpoint tests (39 tests): health probes, auth, content validation, like/unlike, counts, statuses, batch operations, pagination, leaderboard, metrics, SSE, request-id propagation, rate limiting, multi-content-type support
- Integration tests (24 tests): full lifecycle, idempotency, auth flows, batch limits, cursor pagination, leaderboard windows, mock service validation, concurrent race conditions, SSE events, rate limiting, circuit breaker behavior
- Graceful shutdown tests (4 tests): drain behavior and signal handling

## Trade-offs

1. **Runtime SQL queries vs compile-time checked**: Chose runtime to avoid requiring a live DB at build time. Trade-off: lose compile-time SQL validation. Mitigated by integration tests.

2. **bb8-redis vs ConnectionManager**: bb8 gives explicit pool size control matching our config. ConnectionManager would be simpler but less controllable.

3. **Separate like_counts table vs COUNT(*)**: Extra write per like/unlike, but batch count reads become O(1) PK lookups. Critical for the 80/20 read/write ratio.

4. **Single mock binary vs per-service**: Simpler Docker setup. In production, each content API would be a separate service.

5. **User status caching deferred**: `CACHE_TTL_USER_STATUS_SECS` config exists for forward-compatibility but user like status is read directly from DB (fast PK lookup on the unique constraint — already index-backed).

## License

MIT
