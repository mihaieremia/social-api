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
└── Dockerfile.mock          # Multi-stage (mock-services)
```

### Data Flow

```
Request -> request_id -> metrics -> rate_limit -> handler
  handler -> auth extractor (Bearer -> Profile API -> user_id)
  handler -> service (business logic)
    service -> cache (Redis, graceful fallback)
    service -> repository (SQLx, writer/reader pools)
    service -> client (Content API via circuit breaker)
    service -> Redis Pub/Sub (SSE broadcasting)
  handler -> JSON response
```

### Key Design Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Like counts | Separate `like_counts` table | O(1) reads, avoids COUNT(*) on hot path. Extra write cost acceptable for 80/20 read/write. |
| Pagination | Cursor-based (timestamp+id) | Index-seekable, stable under concurrent writes, no row skipping at depth. |
| Caching | Redis cache-aside + stampede locks | SET NX with 5s TTL prevents thundering herd on popular content. |
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
    PRIMARY KEY (content_type, content_id)
);
```

### Indexing Strategy

| Index | Covers | Write Cost |
|-------|--------|------------|
| `idx_likes_user_created (user_id, created_at DESC, id DESC)` | Cursor pagination for user's likes | Low — append-only pattern |
| `idx_likes_content (content_type, content_id)` | Count fallback queries | Low |
| `idx_likes_created_at (created_at)` | Time-windowed leaderboard | Low |

## Redis Key Design

| Key | Type | TTL | Purpose |
|-----|------|-----|---------|
| `lc:{type}:{id}` | STRING | 300s | Like count cache |
| `cv:{type}:{id}` | STRING | 3600s/60s | Content validation cache |
| `rl:w:{hash}` | ZSET | 70s | Write rate limit (30/min) |
| `rl:r:{hash}` | ZSET | 70s | Read rate limit (1000/min) |
| `sse:{type}:{id}` | PUB/SUB | - | SSE event channel |

### Cache Consistency

- **Max staleness:** 300s for counts, 3600s for content validation
- **Invalidation:** INCR/DECR on like/unlike (atomic with DB transaction)
- **Stampede protection:** SET NX lock with 5s TTL, losers wait 50ms then retry or hit DB
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
- `social_api_circuit_breaker_state{service}`
- `social_api_sse_connections_active`
- `social_api_likes_total{content_type, operation}`

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
cargo test --workspace

# Integration tests (requires docker compose up)
docker compose up -d
cargo test --test integration_test -p social-api -- --ignored
```

**30 unit tests** covering:
- Cursor encode/decode roundtrip
- Error code -> HTTP status mapping
- Circuit breaker state transitions (7 tests)
- Path normalization for metrics
- TimeWindow parsing, duration, display
- API error serialization

**20 integration tests** covering:
- Full like lifecycle
- Idempotency (like + unlike)
- Authentication and authorization
- Content validation
- Batch operations with size limits
- Cursor pagination
- Leaderboard with time windows
- Metrics endpoint
- Mock service validation

## Trade-offs

1. **Runtime SQL queries vs compile-time checked**: Chose runtime to avoid requiring a live DB at build time. Trade-off: lose compile-time SQL validation. Mitigated by integration tests.

2. **bb8-redis vs ConnectionManager**: bb8 gives explicit pool size control matching our config. ConnectionManager would be simpler but less controllable.

3. **Separate like_counts table vs COUNT(*)**: Extra write per like/unlike, but batch count reads become O(1) PK lookups. Critical for the 80/20 read/write ratio.

4. **Single mock binary vs per-service**: Simpler Docker setup. In production, each content API would be a separate service.

## License

MIT
