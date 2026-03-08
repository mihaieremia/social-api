# Load Testing with k6

One unified test suite covering **HTTP + gRPC + SSE**, all endpoints, all modes.

## Prerequisites

- [k6](https://grafana.com/docs/k6/latest/set-up/install-k6/) installed (`brew install k6` on macOS)
- Docker services running: `make up` (production) or `make up-stress` (stress config)
- Wait for healthy: `docker compose ps`

## Commands

```bash
make k6-smoke           # Quick 60s check — all transports, all endpoints
make k6-load            # Standard 5min load — moderate RPS
make k6-stress          # Sustained high-RPS ramp — all transports
make k6-seed            # Populate DB with massive data (5k users × 500 content)
make k6-comprehensive   # Full suite: seed → stress all endpoints + race conditions
make k6-tune-macos      # Tune macOS TCP limits before stress testing
```

## Modes

| Mode | Duration | Transports | Purpose |
|------|----------|------------|---------|
| `smoke` | 60s | HTTP + gRPC + SSE | CI gate, quick sanity check |
| `load` | 5min | HTTP + gRPC + SSE | Standard load, all endpoints in parallel |
| `stress` | ~20min | HTTP + gRPC + SSE | Ramp to high RPS, find degradation |
| `seed` | 10min | HTTP | Populate DB for comprehensive testing |
| `comprehensive` | ~30min | HTTP + gRPC + SSE | Seed + all reads + writes + race conditions |

## Endpoints Covered

Every mode exercises **all endpoints** through mixed-dispatch functions:

| Function | Endpoints tested |
|----------|-----------------|
| `httpReadMixed` / `grpcReadMixed` | count, status, user_likes, leaderboard (unfiltered + filtered) |
| `httpWriteCycle` / `grpcWriteCycle` | like, unlike (alternating per VU) |
| `httpBatchMixed` / `grpcBatchMixed` | batch_counts (100 items), batch_statuses (100 items) |
| `httpWriteHot` | Like/unlike on same 5 content items (race condition detector) |
| `sseSubscribe` | SSE event stream connections |
| `seedLike` | Random user likes random content (data population) |

## Configuration

Override via env vars or Makefile vars:

```bash
make k6-stress STRESS_DURATION=30m
make k6-comprehensive NUM_USERS=10000 NUM_CONTENT=1000 SEED_DURATION=20m
```

| Variable | Default | Description |
|----------|---------|-------------|
| `BASE_URL` | `http://localhost:8080` | HTTP API base URL |
| `GRPC_HOST` | `localhost:50051` | gRPC host:port |
| `NUM_USERS` | `100` (smoke/load/stress) / `5000` (seed/comprehensive) | Dynamic token count (tok_user_1..N) |
| `NUM_CONTENT` | `60` (smoke/load/stress) / `500` (seed/comprehensive) | Content items per type |
| `STRESS_DURATION` | `15m` | Hold duration for stress/comprehensive |
| `SEED_DURATION` | `10m` | Duration of seeding phase |
| `MAX_VUS` | `1000` | Cap on VUs per scenario |

## Ad-hoc Usage

```bash
# Quick smoke with custom VUs
k6 run --vus 5 --duration 10s k6/test.js

# Direct scenario selection
k6 run -e K6_SCENARIO=load -e BASE_URL=http://my-host:8080 k6/test.js
```

## Rate Limiting

The docker-compose stress config disables rate limits (1M reads/min, 100K writes/min).
The test treats HTTP 429 and gRPC ResourceExhausted (code 8) as valid responses —
thresholds only evaluate latency on successful requests.

## macOS Port Exhaustion

High-RPS tests can exhaust ephemeral ports on macOS (~16K range). Run `make k6-tune-macos`
before stress testing to expand the port range and reduce TIME_WAIT duration.

## Architecture

```
k6/test.js          Single file, all modes
  ├── HTTP functions   httpReadMixed, httpWriteCycle, httpBatchMixed, httpWriteHot
  ├── gRPC functions   grpcReadMixed, grpcWriteCycle, grpcBatchMixed
  ├── SSE function     sseSubscribe
  ├── Seed function    seedLike
  └── 5 modes          smoke, load, stress, seed, comprehensive
```
