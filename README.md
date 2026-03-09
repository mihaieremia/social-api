# Social API

Likes microservice for BeInCrypto. Handles likes across multiple content types (`post`, `bonus_hunter`, `top_picks`).

## Quick Start

```bash
make build-docker   # Build images
make up             # Start everything
make health         # Verify ready
```

| Service | URL |
|---------|-----|
| API | http://localhost:8080 |
| Swagger UI | http://localhost:8080/swagger-ui |
| Grafana | http://localhost:3000 |
| Prometheus | http://localhost:9090 |

```bash
# Smoke test
curl -s http://localhost:8080/v1/likes/post/731b0395-4888-4822-b516-05b4b7bf2089/count | jq .
```

Run `make` or `make help` to see all available commands.

## API

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| POST | `/v1/likes` | Yes | Like content |
| DELETE | `/v1/likes/{type}/{id}` | Yes | Unlike content |
| GET | `/v1/likes/{type}/{id}/count` | No | Like count (cached) |
| GET | `/v1/likes/{type}/{id}/status` | Yes | User's like status |
| GET | `/v1/likes/user` | Yes | User's likes (cursor paginated) |
| POST | `/v1/likes/batch/counts` | No | Batch counts (max 100) |
| POST | `/v1/likes/batch/statuses` | Yes | Batch statuses (max 100) |
| GET | `/v1/likes/top` | No | Leaderboard (24h/7d/30d/all) |
| GET | `/v1/likes/stream` | No | SSE live events |
| GET | `/health/live`, `/health/ready` | No | Health probes |
| GET | `/metrics` | No | Prometheus metrics |

### internal gRPC Interface
Available on `GRPC_PORT` (default 50051). Supports high-performance inter-service communication for internal APIs.

Full OpenAPI spec at `/api-docs/openapi.json`.

## Testing

```bash
make test               # Pure unit tests (no containers) — no Docker needed
make test-unit          # All unit tests (lib + bin) — uses testcontainers (needs Docker daemon)
make test-integration   # Integration tests — requires make up first
make test-all           # Unit + integration
make coverage           # Unit + http coverage report (needs Docker daemon)
make coverage-full      # Full coverage including docker-compose integration
```

## Load Testing

Requires [k6](https://grafana.com/docs/k6/latest/set-up/install-k6/) (`brew install k6`).

Run `make up-stress` first (disables rate limits, oversized pools).

```bash
make k6-smoke           # Quick 60s smoke test — all transports, all endpoints, low RPS
make k6-load            # Standard 5min load — HTTP+gRPC+SSE parallel, moderate RPS
make k6-stress          # Sustained high-RPS ramp — all transports, STRESS_DURATION hold
make k6-seed            # Seed DB with massive data (NUM_USERS × NUM_CONTENT likes)
make k6-comprehensive   # Full suite: seed → stress all endpoints + race conditions
make k6-tune-macos      # Tune macOS TCP/socket limits for high-RPS tests (requires sudo)
```

Override defaults: `make k6-stress STRESS_DURATION=30m NUM_USERS=100 NUM_CONTENT=60`

See [k6/README.md](k6/README.md) for thresholds and baseline results.

## Configuration

All via environment variables. See [.env.example](.env.example) for the full list.

Required:
```
DATABASE_URL, READ_DATABASE_URL, REDIS_URL, HTTP_PORT
PROFILE_API_URL
At least one CONTENT_API_{TYPE}_URL (e.g., CONTENT_API_POST_URL)
```

Optional:
```
INTERNAL_TRANSPORT (http|grpc), GRPC_PORT
```

Adding a new content type requires only a `CONTENT_API_{TYPE}_URL` env var.

## Setup

```bash
make setup-hooks        # Install pre-commit hook (fmt + clippy gate)
make fmt                # Format code
make lint               # Clippy with deny-warnings
```

## Architecture

```
social-api/
├── crates/
│   ├── social-api/          # Main service binary
│   ├── mock-services/       # Mock APIs (profile + content) on :8081
│   └── shared/              # Common types, errors, cursor
├── migrations/              # SQLx migrations
├── k6/                      # Load tests
├── monitoring/              # Grafana dashboards, Prometheus config, alert rules
└── docker-compose.yml       # Dev stack (Postgres, Redis, mock APIs, observability)
```

Key design choices:
- **Materialized counters** — `like_counts` table avoids COUNT(*) on reads. Batch lookups are O(1) PK scans.
- **Cursor pagination** — base64url-encoded (timestamp+id), index-seekable, stable under concurrent writes.
- **Cache-aside + stampede coalescing** — On cache miss, a watch-channel ensures only one task hits DB; concurrent waiters subscribe and get the result instantly.
- **Rate limiting** — Sliding window via atomic Lua script (ZREMRANGEBYSCORE + ZCARD + ZADD). Write: 30/min per user, Read: 1000/min per IP.
- **Circuit breaker** — For Profile API and Content API calls. Closed->Open after 5 failures, recovers after 30s.
- **Redis failure = degraded, not broken** — All cache misses fall through to DB.

See [CLAUDE.md](CLAUDE.md) for full architecture, data flow, schema, and Redis key design.

## License

MIT
