# Observability Stack Design — Prometheus + Grafana

**Date:** 2026-03-05
**Status:** Approved
**Scope:** Docker Compose dev setup with provisioned Grafana dashboards, alerts, and Prometheus scraping

---

## Decision Summary

- **Option 3 selected:** Grafana alerting now, Alertmanager-ready later
- **Single dashboard** with collapsible row sections (7 rows, ~30 panels)
- **Fully provisioned (GitOps-style):** `docker compose up` gives working dashboards + alerts with zero manual clicks
- **Anonymous admin access** in Grafana for dev ergonomics
- **Histogram buckets** instead of summaries for aggregatability and heatmap support
- **Native exporters** for Postgres and Redis (zero codebase changes)

---

## Metrics Inventory

All metrics emitted by the Social API service:

| Metric | Type | Labels |
|---|---|---|
| `social_api_http_requests_total` | counter | method, path, status |
| `social_api_http_request_duration_seconds` | histogram | method, path (buckets: 1ms-2.5s) |
| `social_api_cache_operations_total` | counter | operation, result |
| `social_api_external_calls_total` | counter | service, method, status |
| `social_api_external_call_duration_seconds` | histogram | service, method (buckets: 1ms-2.5s) |
| `social_api_circuit_breaker_state` | gauge | service |
| `social_api_sse_connections_active` | gauge | (none) |
| `social_api_likes_total` | counter | (dead — described but never incremented) |

**Histogram bucket boundaries:** `[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5]`

Rationale: matches observed p99 (~0.4ms) with room to catch degradation up to 2.5s. Bucket boundaries align with alert thresholds (50ms warning, 100ms/250ms critical).

---

## Rust Code Change

**Single file:** `crates/social-api/src/middleware/metrics.rs`

Replace:
```rust
let builder = metrics_exporter_prometheus::PrometheusBuilder::new();
```

With:
```rust
let builder = metrics_exporter_prometheus::PrometheusBuilder::new()
    .set_buckets(&[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5])
    .unwrap();
```

All `metrics::histogram!()` call sites remain untouched. Output changes from pre-computed quantiles (summary) to bucket counters (histogram), enabling:
- `histogram_quantile()` in PromQL (server-side percentile calculation)
- Aggregation across multiple instances
- Heatmap visualization in Grafana

---

## New Directory Structure

```
monitoring/
├── prometheus/
│   └── prometheus.yml
├── grafana/
│   ├── provisioning/
│   │   ├── datasources/
│   │   │   └── prometheus.yml
│   │   ├── dashboards/
│   │   │   └── provider.yml
│   │   └── alerting/
│   │       └── alerts.yml
│   └── dashboards/
│       └── social-api-overview.json
```

---

## Docker Compose Additions

Four new services (zero codebase impact):

| Service | Image | Purpose | Port |
|---|---|---|---|
| `prometheus` | `prom/prometheus:v3` | Scrapes all /metrics endpoints | 9090 |
| `grafana` | `grafana/grafana:11.5.2` | Dashboard + alerting UI | 3000 |
| `postgres-exporter` | `prometheuscommunity/postgres-exporter` | PG stats (connections, tx, tuples, size, deadlocks) | 9187 |
| `redis-exporter` | `oliver006/redis_exporter:v1.67.0` | Redis stats (memory, clients, commands, keyspace) | 9121 |

### Prometheus Scrape Configuration

| Target | Endpoint | Interval |
|---|---|---|
| `social-api:8080/metrics` | App metrics | 15s |
| `postgres-exporter:9187/metrics` | PostgreSQL metrics | 30s |
| `redis-exporter:9121/metrics` | Redis metrics | 15s |
| `prometheus:9090/metrics` | Self-monitoring | 30s |

---

## Dashboard: Social API Overview

**Settings:** Auto-refresh 15s, default time range 1h.

**Template variables:**
- `$content_type` — multi-select dropdown (post, bonus_hunter, top_picks, all) — filters panels to investigate specific content types
- `$interval` — 1m / 5m / 15m aggregation window toggle

### Row 1 — Golden Signals (always visible)

| Panel | Type | Query |
|---|---|---|
| Request Rate | Stat + sparkline | `sum(rate(social_api_http_requests_total[$interval]))` |
| Error Rate % | Stat (red threshold) | `sum(rate(...{status=~"5.."}[$interval])) / sum(rate(...[$interval])) * 100` |
| p50 Latency | Stat | `histogram_quantile(0.5, sum(rate(..._bucket[$interval])) by (le))` |
| p95 Latency | Stat | `histogram_quantile(0.95, ...)` |
| p99 Latency | Stat | `histogram_quantile(0.99, ...)` |
| Active SSE | Stat | `social_api_sse_connections_active` |
| Circuit Breakers | Status map | `social_api_circuit_breaker_state` (green=0, yellow=1, red=2) |

### Row 2 — Business Metrics (collapsible)

| Panel | Type | Query |
|---|---|---|
| Like Rate | Time series | `rate(...{method="POST",path="/v1/likes",status="201"}[$interval])` |
| Unlike Rate | Time series | `rate(...{method="DELETE",status="200"}[$interval])` |
| Likes by Content Type | Time series (stacked) | Breakdown by path content_type segment |
| Batch Request Volume | Time series | batch/counts + batch/statuses rate |
| Rate Limited Requests | Time series | `rate(...{status="429"}[$interval])` |

### Row 3 — HTTP Traffic (collapsible)

| Panel | Type | Query |
|---|---|---|
| RPS by Endpoint | Time series (stacked) | Breakdown by path |
| RPS by Status Code | Time series (stacked) | 2xx vs 4xx vs 5xx |
| Latency by Endpoint (p50/p95/p99) | Time series | One line per quantile per path |
| Latency Heatmap | Heatmap | `sum(rate(..._bucket[$interval])) by (le)` — shows request density across latency buckets |
| Error Rate by Endpoint | Time series | Per-path 4xx/5xx rate |

### Row 4 — Cache Performance (collapsible)

| Panel | Type | Query |
|---|---|---|
| Cache Hit Ratio | Gauge (%) | `hit / (hit + miss) * 100` |
| Cache Ops/sec by Type | Time series (stacked) | get, mget, set, mset_ex, conditional_incr/decr |
| Cache Errors/sec | Time series | Error rate over time |
| Hit vs Miss vs Error | Pie chart | Current distribution |

### Row 5 — External Dependencies (collapsible)

| Panel | Type | Query |
|---|---|---|
| External Call Rate | Time series | profile_api vs content_api |
| External Call Error Rate | Time series | status="error" per service |
| External Call Latency p99 | Time series | `histogram_quantile(0.99, ...)` per service |
| External Call Latency Heatmap | Heatmap | Bucket distribution for external calls |
| Circuit Breaker State Timeline | State timeline | open/half-open/closed transitions over time |

### Row 6 — PostgreSQL (collapsible)

| Panel | Type | Query |
|---|---|---|
| Active Connections | Time series | `pg_stat_activity_count` |
| Transactions/sec | Time series | `rate(pg_stat_database_xact_commit + rollback)` |
| Tuple Operations | Time series | inserts, updates, deletes rate |
| Database Size | Stat | `pg_database_size_bytes` |
| Deadlocks | Time series | `rate(pg_stat_database_deadlocks)` |

### Row 7 — Redis (collapsible)

| Panel | Type | Query |
|---|---|---|
| Memory Usage | Time series + threshold | `redis_memory_used_bytes` |
| Connected Clients | Time series | `redis_connected_clients` |
| Commands/sec | Time series | `rate(redis_commands_processed_total)` |
| Keyspace Hits/Misses | Time series | `redis_keyspace_hits_total` vs `misses` |
| Key Count | Stat | `redis_db_keys` |

---

## Alert Rules

All alerts defined in Grafana provisioning YAML. Evaluated every 1m.

### Critical (page-worthy)

| Alert | PromQL | For |
|---|---|---|
| HighErrorRate | `sum(rate(social_api_http_requests_total{status=~"5.."}[5m])) / sum(rate(social_api_http_requests_total[5m])) * 100 > 2` | 2m |
| CriticalLatencyP99 | `histogram_quantile(0.99, sum(rate(social_api_http_request_duration_seconds_bucket[5m])) by (le)) > 0.25` | 2m |
| CircuitBreakerOpen | `social_api_circuit_breaker_state == 2` | 0m |

### Warning (investigate)

| Alert | PromQL | For |
|---|---|---|
| ElevatedLatencyP95 | `histogram_quantile(0.95, sum(rate(..._bucket[5m])) by (le)) > 0.05` | 3m |
| ElevatedLatencyP99 | `histogram_quantile(0.99, sum(rate(..._bucket[5m])) by (le)) > 0.1` | 3m |
| CacheErrorSpike | `rate(cache{result="error"}[5m]) / rate(cache_total[5m]) > 0.05` | 3m |
| LowCacheHitRate | `sum(rate(...{result="hit"}[5m])) / sum(rate(...{result=~"hit|miss"}[5m])) < 0.9` | 5m |
| ExternalCallFailures | `rate(external{status="error"}[5m]) / rate(external_total[5m]) > 0.1` | 3m |
| ExternalCallLatency | `histogram_quantile(0.99, sum(rate(external_duration_bucket[5m])) by (le)) > 0.2` | 3m |
| HighSSEConnections | `social_api_sse_connections_active > 500` | 1m |
| PostgresHighConnections | `pg_stat_activity_count > 80` | 3m |
| RedisHighMemory | `redis_memory_used_bytes / redis_memory_max_bytes > 0.8` | 5m |

---

## Grafana Configuration

- **Anonymous access:** enabled, org_role = Admin
- **No login required** for dev
- **Datasource:** Prometheus at `http://prometheus:9090`, default, pre-provisioned
- **Dashboard provider:** file-based, watches `/var/lib/grafana/dashboards/`

---

## Implementation Notes

- All new infrastructure is additive — no existing service configs change
- Exporters connect directly to Postgres/Redis — no app-level changes needed for infrastructure metrics
- Dashboard JSON will be ~2000+ lines; provisioned from file, editable in UI, re-exportable
- Alert contact point defaults to Grafana's built-in (visible in UI). Production would add Slack/email/webhook.
- Recording rules can be added to `prometheus/` later for Alertmanager migration
