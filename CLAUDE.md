# Social API -- Rust Microservice

You are a senior Rust lead building a production-ready Social API microservice for BeInCrypto.

## Spec Reference
All requirements are in `Social API — Rust Microservice.md`. Follow it exactly.

## Architecture

### Workspace Layout
```
social-api/                      # Root
├── Cargo.toml                   # Workspace: members = ["crates/*"]
├── crates/
│   ├── social-api/              # Main service binary
│   ├── mock-services/           # Single binary serving all mock APIs
│   └── shared/                  # Common types, error model, cursor
├── migrations/                  # SQLx migrations (embedded)
├── docker-compose.yml
├── Dockerfile                   # Multi-stage (social-api)
├── Dockerfile.mock              # Multi-stage (mock-services)
├── k6/                          # Load tests (bonus)
└── proto/                       # gRPC definitions (bonus)
```

### Module Structure (crates/social-api/src/)
```
main.rs                          # Entry: config, pools, server, shutdown
config.rs                        # Env-based config with fail-fast
server.rs                        # Axum Router + middleware layers
state.rs                         # AppState (Arc<Inner> with pools, services, config)
shutdown.rs                      # SIGTERM handler, drain, SSE close

handlers/                        # HTTP layer (thin, delegates to services)
  likes.rs                       # like, unlike, get_count, get_status
  batch.rs                       # batch_counts, batch_statuses
  user_likes.rs                  # paginated user likes
  leaderboard.rs                 # top liked content
  stream.rs                      # SSE endpoint
  health.rs                      # /health/live, /health/ready

extractors/                      # Axum custom extractors
  auth.rs                        # AuthUser (Bearer -> user_id via Profile API)
  content_path.rs                # ContentPath (validates type + id)
  pagination.rs                  # CursorParams

middleware/                      # Tower layers
  request_id.rs                  # X-Request-Id generation
  logging.rs                     # Structured request/response logging
  metrics.rs                     # Prometheus collection
  rate_limit.rs                  # Redis sliding window

services/                        # Business logic (no HTTP concerns)
  like_service.rs                # Core like/unlike, count, status, batch
  leaderboard_service.rs         # ZSET reads, DB fallback
  stream_service.rs              # Redis Pub/Sub -> broadcast
  health_service.rs              # Dependency checks

repositories/                    # Data access (raw SQL via sqlx)
  like_repository.rs             # CRUD on likes table
  like_count_repository.rs       # Atomic INCR/DECR on like_counts
  leaderboard_repository.rs      # Time-windowed aggregation

cache/                           # Redis cache layer
  manager.rs                     # Get/set/del with graceful fallback
  like_count_cache.rs            # Single + batch, stampede protection
  content_cache.rs               # Content validation caching
  leaderboard_cache.rs           # Redis Sorted Sets

clients/                         # External HTTP clients
  content_client.rs              # ContentValidator trait + HTTP impl
  profile_client.rs              # TokenValidator trait + HTTP impl
  circuit_breaker.rs             # Generic state machine

domain/                          # Framework-free domain types
  content_type.rs                # Registry (config-driven, zero-code addition)
  like.rs                        # Like entity, LikeEvent
  cursor.rs                      # Base64 encode/decode (timestamp+id)
  time_window.rs                 # TimeWindow enum
  user.rs                        # AuthenticatedUser

errors/
  app_error.rs                   # AppError -> HTTP response mapping

metrics/
  registry.rs                    # Prometheus definitions

tasks/                           # Background tokio tasks
  leaderboard_refresh.rs         # Periodic ZSET rebuild from DB
  cache_warmer.rs                # Startup warm for leaderboard
```

### Data Flow
```
Request -> rate_limit -> request_id -> logging -> metrics -> handler
  handler -> extractor (auth/content validation)
  handler -> service (business logic)
    service -> cache (Redis, graceful fallback)
    service -> repository (SQLx, writer/reader pools)
    service -> client (external APIs via circuit breaker)
    service -> event broadcaster (Redis Pub/Sub for SSE)
  handler -> JSON response
```

## Database Schema

### likes table
```sql
CREATE TABLE likes (
    id           BIGSERIAL PRIMARY KEY,
    user_id      UUID NOT NULL,
    content_type VARCHAR(50) NOT NULL,
    content_id   UUID NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT uq_likes_user_content UNIQUE (user_id, content_type, content_id)
);
-- User's liked items (cursor pagination)
CREATE INDEX idx_likes_user_created ON likes (user_id, created_at DESC, id DESC);
-- Count aggregation fallback
CREATE INDEX idx_likes_content ON likes (content_type, content_id);
-- Time-windowed leaderboard
CREATE INDEX idx_likes_created_at ON likes (created_at);
```

### like_counts table (materialized counter)
```sql
CREATE TABLE like_counts (
    content_type VARCHAR(50) NOT NULL,
    content_id   UUID NOT NULL,
    total_count  BIGINT NOT NULL DEFAULT 0,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (content_type, content_id),
    CHECK (total_count >= 0)
);
```

**Why like_counts table:** Avoids COUNT(*) on every read. Batch counts (100 items) becomes a simple PK lookup. Write cost is one extra UPDATE per like/unlike -- acceptable for 80/20 read/write ratio.

## Redis Key Design

| Key Pattern | Type | TTL | Purpose |
|---|---|---|---|
| `lc:{type}:{id}` | STRING | 300s | Like count (INCR/DECR atomic) |
| `cv:{type}:{id}` | STRING | 3600s valid, 60s invalid | Content validation cache |
| `rl:w:{user_id}` | ZSET | 120s | Write rate limit (sliding window) |
| `rl:r:{ip}` | ZSET | 120s | Read rate limit (sliding window) |
| `lb:{window}:{type}` | ZSET | 120s | Leaderboard (score=count) |
| `sse:{type}:{id}` | PUB/SUB | -- | SSE event channel |

### Stampede Protection
Lock-based (SET NX with 5s TTL). On cache miss: acquire lock, fetch from DB, populate cache, release. Losers wait 50ms and retry cache, then fallback to DB.

### Cache Warming
On startup: trigger leaderboard refresh for all windows before readiness probe returns 200. Like counts use lazy population (cache-aside on first access).

## Key Traits (Extensibility)

```rust
// Adding new content type = config only (env var)
pub struct ContentTypeRegistry { types: HashMap<String, ContentTypeConfig> }

// Transport-swappable (HTTP today, gRPC tomorrow)
#[async_trait]
pub trait ContentValidator: Send + Sync { ... }
#[async_trait]
pub trait TokenValidator: Send + Sync { ... }

// Testable via mockall
#[async_trait]
pub trait LikeRepository: Send + Sync { ... }
#[async_trait]
pub trait LikeCountRepository: Send + Sync { ... }
#[async_trait]
pub trait LikeCountCache: Send + Sync { ... }
#[async_trait]
pub trait EventBroadcaster: Send + Sync { ... }
```

## Cursor Pagination
Encodes `{"t":"2026-02-02T17:00:00Z","id":12345}` as base64url. Query uses `WHERE (created_at, id) < ($cursor_ts, $cursor_id)` for index-seekable pagination. Fetch limit+1 rows to detect has_more.

**Why cursor over offset:** No row skipping at depth, stable under concurrent writes, no COUNT(*) needed.

## Rules

1. **Task-based commits:** Each task gets one commit with subject + description
2. **Modularity:** Small functions, single responsibility, easy to test
3. **Handler pattern:** Handlers are thin -- deserialize, call service, serialize. No business logic in handlers.
4. **Error handling:** All errors through AppError enum -> consistent JSON envelope with error code, message, request_id, details
5. **Cache graceful degradation:** Redis failures return None, never propagate errors to caller. Service continues with degraded performance.
6. **No unwrap/expect in production code.** Use `?` operator and proper error propagation.
7. **Structured logging:** Every log line: timestamp, level, message, request_id, service. Requests: method, path, status, latency_ms. External calls: service, method, latency_ms, success.
8. **SQL via SQLx:** Compile-time checked queries. Writer pool for mutations, reader pool for reads.
9. **Atomic count updates:** Like/unlike wraps INSERT/DELETE + count UPDATE in a single transaction.
10. **Rate limiting via Lua script:** ZREMRANGEBYSCORE + ZCARD + ZADD in atomic Redis script for sliding window correctness.

## Key Dependencies

| Crate | Purpose |
|---|---|
| axum 0.8 | Web framework |
| sqlx 0.8 (postgres, migrate) | Database |
| redis + bb8-redis | Cache + pub/sub |
| reqwest | HTTP client |
| tracing + tracing-subscriber | Structured logging |
| metrics + metrics-exporter-prometheus | Metrics |
| serde + serde_json | Serialization |
| uuid, chrono, base64, thiserror | Utilities |
| tokio (full) | Async runtime |
| tower + tower-http | Middleware |

## Docker

- **Dockerfile:** Multi-stage (rust:latest builder -> debian:bookworm-slim). Non-root user. HEALTHCHECK.
- **docker-compose.yml:** social-api, postgres:16, redis:7, mock-services (4 aliases on ports 8081-8084). depends_on with health checks.
- **`docker compose up --build`** starts everything.
