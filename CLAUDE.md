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
├── Dockerfile                   # Multi-target (social-api + mock-services, distroless runtime)
└── k6/                          # Load tests
```

### Module Structure (crates/social-api/src/)
```
main.rs                          # Entry: config, pools, server, shutdown
state.rs                         # AppState (Arc<AppStateInner>: db, cache, config, http_client, like_service, token_validator, profile_breaker, pubsub_manager, shutdown_token, inflight_count)
shutdown.rs                      # SIGTERM handler, drain, SSE close
db.rs                            # DbPools (writer + reader PgPool)
logging.rs                       # JSON tracing-subscriber init
openapi.rs                       # utoipa OpenAPI spec + Swagger UI
proto.rs                         # Compiled protobuf module entrypoint

auth.rs                          # Shared auth flow for HTTP extractors and gRPC interceptors
cache.rs                         # CacheManager: get/set/mget/publish/zrevrange/replace_sorted_set/invoke_script
config.rs                        # Config struct + env-backed loading + env parsing helpers
content.rs                       # Config-backed content type validation helpers
health.rs                        # Shared dependency checks for HTTP + gRPC health
realtime.rs                      # Shared SSE/gRPC event stream bridge
repositories.rs                  # SQL for likes, counts, statuses, user likes, leaderboard
server.rs                        # Axum Router + HTTP middleware layers + tonic server wiring

handlers/
  mod.rs                         # ApiErrorResponse (Axum AppError -> JSON response adapter)
  likes.rs                       # HTTP like endpoints: like, unlike, count, status, user likes, batch, leaderboard
  stream.rs                      # SSE endpoint
  health.rs                      # /health/live, /health/ready
  metrics_handler.rs             # /metrics (PrometheusHandle)

extractors/
  auth.rs                        # AuthUser (Bearer token -> circuit-breaker -> Profile API -> user_id)
  content_path.rs                # ContentPath (validates type in registry + id as UUID)

middleware/
  request_id.rs                  # X-Request-Id generation
  metrics.rs                     # Prometheus HTTP metrics collection
  rate_limit.rs                  # Lua sliding-window rate limit (write=per-token-hash, read=per-IP-hash)
  inflight.rs                    # In-flight counter for graceful shutdown drain
  error_context.rs               # Patches request_id into error responses after handler runs

services/
  like_service.rs                # Facade over like command/query/cache modules
  like_commands.rs               # Like/unlike write workflows
  like_queries.rs                # Read paths, pagination, leaderboard queries
  like_count_cache.rs            # Count cache + stampede coalescing
  like_events.rs                 # Event publication helpers
  pubsub_manager.rs              # Redis Pub/Sub subscription manager for SSE fan-out

grpc/
  likes_service.rs               # gRPC LikeService transport layer
  health_service.rs              # gRPC health transport layer
  convert.rs                     # Domain <-> protobuf conversion helpers
  error.rs                       # AppError -> tonic::Status mapping
  interceptors/                  # gRPC request ID, auth, rate-limit, metrics helpers

clients/
  content_client.rs              # ContentValidator trait + HttpContentValidator
  profile_client.rs              # TokenValidator trait + HttpTokenValidator
  circuit_breaker.rs             # Generic state machine (Closed/HalfOpen/Open)
  grpc_profile_client.rs         # GrpcTokenValidator (tonic)
  grpc_content_client.rs         # GrpcContentValidator (tonic)
  metrics.rs                     # Helpers for recording external call metrics

tasks/
  leaderboard_refresh.rs         # Periodic ZSET rebuild; also warms cache on startup
  db_pool_metrics.rs             # Periodic DB pool gauge emission

shared crate (crates/shared/src/):
  types.rs                       # All domain types: AuthenticatedUser, TimeWindow, Like, LikeEvent, request/response structs
  errors.rs                      # AppError enum, ErrorCode, ApiError JSON envelope
  cursor.rs                      # Base64url encode/decode (timestamp+id)
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
-- Time-windowed leaderboard with content_type filter support
-- Filtered queries seek to content_type partition, then range-scan created_at.
CREATE INDEX idx_likes_ct_created_cid ON likes (content_type, created_at DESC, content_id);
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
| `lc:{type}:{id}` | STRING | 300s | Like count (write-through on mutations, cache-aside on reads) |
| `cv:{type}:{id}` | STRING | 3600s valid, 60s invalid | Content validation cache |
| `rl:w:{fnv1a_hash(authorization_header)}` | ZSET | window+10s | Write rate limit (30/min sliding window) |
| `rl:r:{fnv1a_hash(ip)}` | ZSET | window+10s | Read rate limit (1000/min sliding window) |
| `lb:{window}` | ZSET | no TTL | Global leaderboard per window (score=count, member="{type}:{id}"); content_type filter in app |
| `ls:{user}:{type}:{id}` | STRING | 300s | Per-user like status (RFC3339 ts = liked, "" = not liked) |
| `ul:{user}:{filter}` | STRING | 60s | User likes first page (JSON, cursor-less only) |
| `lbf:{window}:{type}:{limit}` | STRING | 60s | Filtered leaderboard (JSON) |
| `sse:{type}:{id}` | PUB/SUB | -- | SSE event channel |

### Stampede Protection
Watch-channel coalescing via `DashMap<String, watch::Sender>`. On count cache miss: first task registers a `watch::Sender` in `pending_fetches` and queries DB; concurrent tasks subscribe to the same sender and receive the result instantly. If the fetcher fails, waiters fall back to DB directly.

### Cache Warming
On startup: leaderboard refresh task runs immediately before entering the periodic loop, populating `lb:{window}` ZSETs for all time windows (24h, 7d, 30d, all). Like counts use lazy population (cache-aside on first access).

## Key Traits (Extensibility)

```rust
// Transport-swappable (HTTP and gRPC, selected via INTERNAL_TRANSPORT env var)
#[async_trait]
pub trait ContentValidator: Send + Sync { ... }
#[async_trait]
pub trait TokenValidator: Send + Sync { ... }
```

Repositories are plain async functions (not traits). Cache is a concrete `CacheManager` struct (not a trait). Content type registry is config-driven: adding a new type requires only a `CONTENT_API_{TYPE}_URL` env var — zero code changes.

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
8. **SQL via SQLx:** Runtime queries (`sqlx::query` / `sqlx::query_as`, not compile-time macros — avoids requiring a live DB at build time). Writer pool for mutations, reader pool for reads.
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
| utoipa + utoipa-swagger-ui | OpenAPI spec + Swagger UI |

## Docker

- **Dockerfile:** Multi-target (shared rust:1.90 builder -> gcr.io/distroless/cc-debian13:nonroot runtime). Targets: `social-api`, `mock-services`. Non-root (distroless uid 65534).
- **docker-compose.yml:** social-api, postgres:16, redis:7-alpine, mock-services (one binary on port 8081 serves all content APIs and profile API). depends_on with health checks. Each service uses `target:` to select its Dockerfile stage. Monitoring services use `profiles: [monitoring]`.
- **`make up`** starts everything including monitoring. **`make up-app`** starts app stack only (`make help` for all commands).
