# Load Testing with k6

## Prerequisites

- [k6](https://grafana.com/docs/k6/latest/set-up/install-k6/) installed (`brew install k6` on macOS)
- Docker services running: `docker compose up --build -d`
- Wait for healthy: `docker compose ps` (all services should show "healthy")

## Run All Scenarios (Sequential)

```bash
k6 run k6/load_test.js
```

Scenarios run sequentially: read (60s) -> batch (60s) -> write (60s) -> mixed (120s).
Total duration: ~6 minutes.

## Run Individual Scenarios

```bash
# Read path only (10k rps target)
k6 run -e K6_SCENARIO=read_path k6/load_test.js

# Batch path only (1k rps target)
k6 run -e K6_SCENARIO=batch_path k6/load_test.js

# Write path only (500 rps target)
k6 run -e K6_SCENARIO=write_path k6/load_test.js

# Mixed workload only (2k rps, 80/15/5 ratio)
k6 run -e K6_SCENARIO=mixed k6/load_test.js
```

## Quick Smoke Test

```bash
# Low-rate mixed workload (uses default export)
k6 run --vus 5 --duration 10s k6/load_test.js
```

## Custom Base URL

```bash
k6 run -e BASE_URL=http://your-host:9080 k6/load_test.js
```

## Target Thresholds

| Scenario   | Rate       | p99 Target |
|------------|------------|------------|
| read_path  | 10,000 rps | < 5ms      |
| batch_path | 1,000 rps  | < 50ms     |
| write_path | 500 rps    | < 100ms    |
| mixed      | 2,000 rps  | < 100ms    |
| All        | error rate | < 1%       |

## Rate Limiting

The docker-compose.yml sets high rate limits (1M reads/min, 100K writes/min) so
the load test can exercise throughput without being capped. For realistic rate
limit testing, lower these values in docker-compose.yml:

```yaml
RATE_LIMIT_READ_PER_MINUTE: "1000"
RATE_LIMIT_WRITE_PER_MINUTE: "30"
```

The k6 checks treat HTTP 429 (rate limited) as a valid response, so the test
will still pass even with aggressive rate limits — only latency thresholds will
be evaluated on successful requests.

## Baseline Results (2026-03-05)

Environment: Docker Compose on macOS (Apple Silicon), single machine.
All 5 thresholds passed. Zero failures across 929,991 requests.

### Thresholds

| Threshold | Target | Actual | Status |
|-----------|--------|--------|--------|
| read_path p99 | < 5ms | 1.45ms | PASS |
| batch_path p99 | < 50ms | 2.01ms | PASS |
| write_path p99 | < 100ms | 2.39ms | PASS |
| mixed p99 | < 100ms | 1.63ms | PASS |
| Error rate | < 1% | 0.00% | PASS |

### Latency

| Scenario | Rate | avg | med | p90 | p95 | p99 | max |
|----------|------|-----|-----|-----|-----|-----|-----|
| read_path | 10k rps | 601us | 571us | 704us | 777us | 1.45ms | 25ms |
| batch_path | 1k rps | 700us | 598us | 774us | 956us | 2.01ms | 71ms |
| write_path | 500 rps | 1.4ms | 1.4ms | 1.72ms | 1.86ms | 2.39ms | 16ms |
| mixed (80/15/5) | 2k rps | 462us | 421us | 590us | 833us | 1.63ms | 9.5ms |

### Summary

| Metric | Value |
|--------|-------|
| Total requests | 929,991 |
| Sustained throughput | 2,818 req/s |
| Checks passed | 1,859,982 / 1,859,982 (100%) |
| Data received | 736 MB |
| Data sent | 513 MB |
| Dropped iterations | 10 (0.001%) |
| Max VUs used | 13 of 210 allocated |
| Duration | 5m30s |

## Notes

- Uses `constant-arrival-rate` executor to guarantee exact rps regardless of response time.
- Write path alternates like/unlike to generate real DB writes every request.
- Mock data: 20 users x 20 content IDs x 3 types = 1,200 unique combinations.
- Running on Docker (single machine) will likely not hit 10k rps p99 < 5ms. These thresholds are designed for production or dedicated load testing infrastructure.
