# Observability Stack Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add Prometheus + Grafana observability stack with provisioned dashboards, alerts, and Postgres/Redis exporters to the docker-compose dev environment.

**Architecture:** Four new docker-compose services (Prometheus, Grafana, postgres-exporter, redis-exporter) with file-based provisioning. One 2-line Rust change switches histogram output from summaries to buckets. All config lives under `monitoring/`.

**Tech Stack:** Prometheus v3, Grafana 11.5.2, postgres-exporter, redis_exporter, metrics-exporter-prometheus (Rust)

**Design doc:** `docs/plans/2026-03-05-observability-stack-design.md`

---

### Task 1: Switch histograms from summaries to buckets

**Files:**
- Modify: `crates/social-api/src/middleware/metrics.rs:51-55`

**Step 1: Modify the PrometheusBuilder to use explicit histogram buckets**

In `crates/social-api/src/middleware/metrics.rs`, change the `init_metrics` function.

Replace lines 52-55:
```rust
    let builder = metrics_exporter_prometheus::PrometheusBuilder::new();
    let handle = builder
        .install_recorder()
        .expect("Failed to install Prometheus recorder");
```

With:
```rust
    let builder = metrics_exporter_prometheus::PrometheusBuilder::new()
        .set_buckets(&[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5])
        .expect("Invalid bucket boundaries");
    let handle = builder
        .install_recorder()
        .expect("Failed to install Prometheus recorder");
```

**Step 2: Verify existing tests still pass**

Run: `cargo test -p social-api -- middleware::metrics`
Expected: All 3 `normalize_path` tests pass. No other tests affected since `init_metrics` can only be called once per process (global recorder).

**Step 3: Verify compilation**

Run: `cargo build -p social-api`
Expected: Clean build. The `set_buckets` method is available on `PrometheusBuilder` in `metrics-exporter-prometheus 0.16`.

**Step 4: Commit**

```
feat(metrics): switch histogram output from summaries to buckets

Use explicit bucket boundaries [1ms, 5ms, 10ms, 25ms, 50ms, 100ms,
250ms, 500ms, 1s, 2.5s] instead of default summary quantiles.
Enables histogram_quantile() in PromQL, cross-instance aggregation,
and Grafana heatmap visualization.
```

---

### Task 2: Create Prometheus configuration

**Files:**
- Create: `monitoring/prometheus/prometheus.yml`

**Step 1: Create directory structure**

Run: `mkdir -p monitoring/prometheus`

**Step 2: Create Prometheus config**

Create `monitoring/prometheus/prometheus.yml`:
```yaml
global:
  scrape_interval: 15s
  evaluation_interval: 15s

scrape_configs:
  - job_name: "social-api"
    scrape_interval: 15s
    metrics_path: /metrics
    static_configs:
      - targets: ["social-api:8080"]
        labels:
          service: "social-api"

  - job_name: "postgres-exporter"
    scrape_interval: 30s
    static_configs:
      - targets: ["postgres-exporter:9187"]
        labels:
          service: "postgres"

  - job_name: "redis-exporter"
    scrape_interval: 15s
    static_configs:
      - targets: ["redis-exporter:9121"]
        labels:
          service: "redis"

  - job_name: "prometheus"
    scrape_interval: 30s
    static_configs:
      - targets: ["localhost:9090"]
        labels:
          service: "prometheus"
```

**Step 3: Commit**

```
feat(monitoring): add Prometheus scrape configuration

Configures scrape targets for social-api (15s), postgres-exporter (30s),
redis-exporter (15s), and Prometheus self-monitoring (30s).
```

---

### Task 3: Create Grafana provisioning configs

**Files:**
- Create: `monitoring/grafana/provisioning/datasources/prometheus.yml`
- Create: `monitoring/grafana/provisioning/dashboards/provider.yml`

**Step 1: Create directory structure**

Run: `mkdir -p monitoring/grafana/provisioning/datasources monitoring/grafana/provisioning/dashboards monitoring/grafana/provisioning/alerting monitoring/grafana/dashboards`

**Step 2: Create datasource provisioning**

Create `monitoring/grafana/provisioning/datasources/prometheus.yml`:
```yaml
apiVersion: 1

datasources:
  - name: Prometheus
    type: prometheus
    access: proxy
    url: http://prometheus:9090
    isDefault: true
    editable: true
    jsonData:
      httpMethod: POST
      timeInterval: "15s"
```

**Step 3: Create dashboard provider**

Create `monitoring/grafana/provisioning/dashboards/provider.yml`:
```yaml
apiVersion: 1

providers:
  - name: "Social API"
    orgId: 1
    folder: ""
    type: file
    disableDeletion: false
    editable: true
    updateIntervalSeconds: 10
    allowUiUpdates: true
    options:
      path: /var/lib/grafana/dashboards
      foldersFromFilesStructure: false
```

**Step 4: Commit**

```
feat(monitoring): add Grafana datasource and dashboard provisioning

Auto-registers Prometheus as default datasource and configures
file-based dashboard provider from /var/lib/grafana/dashboards.
```

---

### Task 4: Create Grafana alert rules

**Files:**
- Create: `monitoring/grafana/provisioning/alerting/alerts.yml`

**Step 1: Create alert rules file**

Create `monitoring/grafana/provisioning/alerting/alerts.yml`:
```yaml
apiVersion: 1

groups:
  - orgId: 1
    name: social-api-critical
    folder: Social API
    interval: 1m
    rules:
      - uid: high-error-rate
        title: "High Error Rate"
        condition: C
        data:
          - refId: A
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: prometheus
            model:
              expr: sum(rate(social_api_http_requests_total{status=~"5.."}[5m]))
              instant: true
              refId: A
          - refId: B
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: prometheus
            model:
              expr: sum(rate(social_api_http_requests_total[5m]))
              instant: true
              refId: B
          - refId: C
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: __expr__
            model:
              type: math
              expression: "($A / $B) * 100"
              refId: C
              conditions:
                - evaluator:
                    type: gt
                    params: [2]
        for: 2m
        labels:
          severity: critical
        annotations:
          summary: "5xx error rate exceeds 2% over 5 minutes"
          dashboard_row: "HTTP Traffic"

      - uid: critical-latency-p99
        title: "Critical Latency P99"
        condition: A
        data:
          - refId: A
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: prometheus
            model:
              expr: histogram_quantile(0.99, sum(rate(social_api_http_request_duration_seconds_bucket[5m])) by (le))
              instant: true
              refId: A
        for: 2m
        labels:
          severity: critical
        annotations:
          summary: "P99 latency exceeds 250ms for 2 minutes"
          dashboard_row: "HTTP Traffic"

      - uid: circuit-breaker-open
        title: "Circuit Breaker Open"
        condition: A
        data:
          - refId: A
            relativeTimeRange:
              from: 60
              to: 0
            datasourceUid: prometheus
            model:
              expr: social_api_circuit_breaker_state == 2
              instant: true
              refId: A
        for: 0s
        labels:
          severity: critical
        annotations:
          summary: "Circuit breaker is OPEN — external dependency is down"
          dashboard_row: "External Dependencies"

  - orgId: 1
    name: social-api-warning
    folder: Social API
    interval: 1m
    rules:
      - uid: elevated-latency-p95
        title: "Elevated Latency P95"
        condition: A
        data:
          - refId: A
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: prometheus
            model:
              expr: histogram_quantile(0.95, sum(rate(social_api_http_request_duration_seconds_bucket[5m])) by (le))
              instant: true
              refId: A
        for: 3m
        labels:
          severity: warning
        annotations:
          summary: "P95 latency exceeds 50ms for 3 minutes"

      - uid: elevated-latency-p99
        title: "Elevated Latency P99"
        condition: A
        data:
          - refId: A
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: prometheus
            model:
              expr: histogram_quantile(0.99, sum(rate(social_api_http_request_duration_seconds_bucket[5m])) by (le))
              instant: true
              refId: A
        for: 3m
        labels:
          severity: warning
        annotations:
          summary: "P99 latency exceeds 100ms for 3 minutes"

      - uid: cache-error-spike
        title: "Cache Error Spike"
        condition: C
        data:
          - refId: A
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: prometheus
            model:
              expr: sum(rate(social_api_cache_operations_total{result="error"}[5m]))
              instant: true
              refId: A
          - refId: B
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: prometheus
            model:
              expr: sum(rate(social_api_cache_operations_total[5m]))
              instant: true
              refId: B
          - refId: C
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: __expr__
            model:
              type: math
              expression: "$A / $B"
              refId: C
              conditions:
                - evaluator:
                    type: gt
                    params: [0.05]
        for: 3m
        labels:
          severity: warning
        annotations:
          summary: "Cache error rate exceeds 5% over 5 minutes"

      - uid: low-cache-hit-rate
        title: "Low Cache Hit Rate"
        condition: C
        data:
          - refId: A
            relativeTimeRange:
              from: 600
              to: 0
            datasourceUid: prometheus
            model:
              expr: sum(rate(social_api_cache_operations_total{result="hit"}[10m]))
              instant: true
              refId: A
          - refId: B
            relativeTimeRange:
              from: 600
              to: 0
            datasourceUid: prometheus
            model:
              expr: sum(rate(social_api_cache_operations_total{result=~"hit|miss"}[10m]))
              instant: true
              refId: B
          - refId: C
            relativeTimeRange:
              from: 600
              to: 0
            datasourceUid: __expr__
            model:
              type: math
              expression: "$A / $B"
              refId: C
              conditions:
                - evaluator:
                    type: lt
                    params: [0.9]
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Cache hit rate below 90% over 10 minutes"

      - uid: external-call-failures
        title: "External Call Failures"
        condition: C
        data:
          - refId: A
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: prometheus
            model:
              expr: sum(rate(social_api_external_calls_total{status="error"}[5m]))
              instant: true
              refId: A
          - refId: B
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: prometheus
            model:
              expr: sum(rate(social_api_external_calls_total[5m]))
              instant: true
              refId: B
          - refId: C
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: __expr__
            model:
              type: math
              expression: "$A / $B"
              refId: C
              conditions:
                - evaluator:
                    type: gt
                    params: [0.1]
        for: 3m
        labels:
          severity: warning
        annotations:
          summary: "External call error rate exceeds 10% over 5 minutes"

      - uid: external-call-latency
        title: "External Call Latency"
        condition: A
        data:
          - refId: A
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: prometheus
            model:
              expr: histogram_quantile(0.99, sum(rate(social_api_external_call_duration_seconds_bucket[5m])) by (le))
              instant: true
              refId: A
        for: 3m
        labels:
          severity: warning
        annotations:
          summary: "External call P99 latency exceeds 200ms for 3 minutes"

      - uid: high-sse-connections
        title: "High SSE Connections"
        condition: A
        data:
          - refId: A
            relativeTimeRange:
              from: 60
              to: 0
            datasourceUid: prometheus
            model:
              expr: social_api_sse_connections_active
              instant: true
              refId: A
        for: 1m
        labels:
          severity: warning
        annotations:
          summary: "Active SSE connections exceed 500"

      - uid: postgres-high-connections
        title: "PostgreSQL High Connections"
        condition: A
        data:
          - refId: A
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: prometheus
            model:
              expr: pg_stat_activity_count
              instant: true
              refId: A
        for: 3m
        labels:
          severity: warning
        annotations:
          summary: "PostgreSQL active connections exceed 80"

      - uid: redis-high-memory
        title: "Redis High Memory"
        condition: C
        data:
          - refId: A
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: prometheus
            model:
              expr: redis_memory_used_bytes
              instant: true
              refId: A
          - refId: B
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: prometheus
            model:
              expr: redis_memory_max_bytes
              instant: true
              refId: B
          - refId: C
            relativeTimeRange:
              from: 300
              to: 0
            datasourceUid: __expr__
            model:
              type: math
              expression: "$A / $B"
              refId: C
              conditions:
                - evaluator:
                    type: gt
                    params: [0.8]
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Redis memory usage exceeds 80%"

contactPoints:
  - orgId: 1
    name: default
    receivers:
      - uid: default-email
        type: email
        settings:
          addresses: admin@localhost
          singleEmail: true

policies:
  - orgId: 1
    receiver: default
    group_by: ["alertname", "severity"]
    group_wait: 30s
    group_interval: 5m
    repeat_interval: 4h
```

**Step 2: Commit**

```
feat(monitoring): add Grafana alert rules

12 alert rules (3 critical, 9 warning) covering error rate, latency,
cache health, external dependencies, circuit breakers, SSE connections,
PostgreSQL connections, and Redis memory. Aggressive thresholds for
early detection: p95 > 50ms warning, p99 > 100ms warning, p99 > 250ms
critical.
```

---

### Task 5: Create Grafana dashboard JSON

**Files:**
- Create: `monitoring/grafana/dashboards/social-api-overview.json`

This is the largest file (~2500 lines). The dashboard has:
- 2 template variables: `$content_type` (multi-select), `$interval` (custom: 1m,5m,15m)
- 7 rows, ~30 panels

**Step 1: Create the dashboard JSON**

Create `monitoring/grafana/dashboards/social-api-overview.json` with the complete dashboard definition.

The JSON structure follows this pattern for each panel:
```json
{
  "type": "timeseries|stat|gauge|heatmap|piechart|state-timeline",
  "title": "Panel Title",
  "gridPos": { "h": 8, "w": 6, "x": 0, "y": 0 },
  "datasource": { "type": "prometheus", "uid": "prometheus" },
  "targets": [
    {
      "expr": "PromQL expression using $interval and $content_type",
      "legendFormat": "{{label}}",
      "refId": "A"
    }
  ],
  "fieldConfig": { ... }
}
```

**Key PromQL queries per row:**

Row 1 — Golden Signals:
- Request Rate: `sum(rate(social_api_http_requests_total[$interval]))`
- Error Rate: `sum(rate(social_api_http_requests_total{status=~"5.."}[$interval])) / sum(rate(social_api_http_requests_total[$interval])) * 100`
- p50/p95/p99: `histogram_quantile(0.X, sum(rate(social_api_http_request_duration_seconds_bucket[$interval])) by (le))`
- SSE: `social_api_sse_connections_active`
- Circuit Breakers: `social_api_circuit_breaker_state`

Row 2 — Business Metrics:
- Like Rate: `sum(rate(social_api_http_requests_total{method="POST",path="/v1/likes",status="201"}[$interval]))`
- Unlike Rate: `sum(rate(social_api_http_requests_total{method="DELETE",path=~"/v1/likes/.*",status="200"}[$interval]))`
- By Content Type: `sum by (path) (rate(social_api_http_requests_total{method=~"POST|DELETE",path=~"/v1/likes/$content_type.*"}[$interval]))`
- Batch Volume: `sum(rate(social_api_http_requests_total{path=~"/v1/likes/batch/.*"}[$interval]))`
- Rate Limited: `sum(rate(social_api_http_requests_total{status="429"}[$interval]))`

Row 3 — HTTP Traffic:
- RPS by endpoint: `sum by (path) (rate(social_api_http_requests_total[$interval]))`
- RPS by status: `sum by (status) (rate(social_api_http_requests_total[$interval]))`
- Latency by endpoint: `histogram_quantile(0.99, sum by (le, path) (rate(social_api_http_request_duration_seconds_bucket[$interval])))`
- Heatmap: `sum(rate(social_api_http_request_duration_seconds_bucket[$interval])) by (le)`
- Error rate by endpoint: `sum by (path) (rate(social_api_http_requests_total{status=~"[45].."}[$interval]))`

Row 4 — Cache:
- Hit ratio: `sum(rate(social_api_cache_operations_total{result="hit"}[$interval])) / sum(rate(social_api_cache_operations_total{result=~"hit|miss"}[$interval])) * 100`
- Ops by type: `sum by (operation) (rate(social_api_cache_operations_total[$interval]))`
- Errors: `sum(rate(social_api_cache_operations_total{result="error"}[$interval]))`

Row 5 — External Dependencies:
- Call rate: `sum by (service) (rate(social_api_external_calls_total[$interval]))`
- Error rate: `sum by (service) (rate(social_api_external_calls_total{status="error"}[$interval]))`
- Latency: `histogram_quantile(0.99, sum by (le, service) (rate(social_api_external_call_duration_seconds_bucket[$interval])))`
- Heatmap: `sum(rate(social_api_external_call_duration_seconds_bucket[$interval])) by (le)`
- Circuit breaker: `social_api_circuit_breaker_state`

Row 6 — PostgreSQL (from postgres-exporter):
- Connections: `pg_stat_activity_count`
- TX/sec: `rate(pg_stat_database_xact_commit{datname="social_api"}[$interval]) + rate(pg_stat_database_xact_rollback{datname="social_api"}[$interval])`
- Tuples: `rate(pg_stat_database_tup_inserted{datname="social_api"}[$interval])`
- DB size: `pg_database_size_bytes{datname="social_api"}`
- Deadlocks: `rate(pg_stat_database_deadlocks{datname="social_api"}[$interval])`

Row 7 — Redis (from redis-exporter):
- Memory: `redis_memory_used_bytes`
- Clients: `redis_connected_clients`
- Commands: `rate(redis_commands_processed_total[$interval])`
- Keyspace: `rate(redis_keyspace_hits_total[$interval])` vs `rate(redis_keyspace_misses_total[$interval])`
- Keys: `redis_db_keys{db="db0"}`

**Step 2: Commit**

```
feat(monitoring): add Grafana dashboard for Social API overview

Provisioned dashboard with 7 rows, ~30 panels: Golden Signals,
Business Metrics, HTTP Traffic (with latency heatmap), Cache
Performance, External Dependencies (with latency heatmap),
PostgreSQL, and Redis. Template variables for content_type and
aggregation interval.
```

---

### Task 6: Update docker-compose.yml

**Files:**
- Modify: `docker-compose.yml`

**Step 1: Add four new services and a Prometheus volume**

Add after the `social-api` service block (before `volumes:`):

```yaml
  prometheus:
    image: prom/prometheus:v3
    ports:
      - "9090:9090"
    volumes:
      - ./monitoring/prometheus/prometheus.yml:/etc/prometheus/prometheus.yml:ro
      - prometheus_data:/prometheus
    command:
      - "--config.file=/etc/prometheus/prometheus.yml"
      - "--storage.tsdb.retention.time=7d"
      - "--web.enable-lifecycle"
    depends_on:
      social-api:
        condition: service_healthy
    healthcheck:
      test: ["CMD", "wget", "--quiet", "--tries=1", "--spider", "http://localhost:9090/-/healthy"]
      interval: 10s
      timeout: 3s
      retries: 3

  grafana:
    image: grafana/grafana:11.5.2
    ports:
      - "3000:3000"
    environment:
      GF_AUTH_ANONYMOUS_ENABLED: "true"
      GF_AUTH_ANONYMOUS_ORG_ROLE: Admin
      GF_AUTH_DISABLE_LOGIN_FORM: "true"
      GF_DASHBOARDS_DEFAULT_HOME_DASHBOARD_PATH: /var/lib/grafana/dashboards/social-api-overview.json
    volumes:
      - ./monitoring/grafana/provisioning:/etc/grafana/provisioning:ro
      - ./monitoring/grafana/dashboards:/var/lib/grafana/dashboards:ro
      - grafana_data:/var/lib/grafana
    depends_on:
      prometheus:
        condition: service_healthy
    healthcheck:
      test: ["CMD", "wget", "--quiet", "--tries=1", "--spider", "http://localhost:3000/api/health"]
      interval: 10s
      timeout: 3s
      retries: 3

  postgres-exporter:
    image: prometheuscommunity/postgres-exporter
    environment:
      DATA_SOURCE_NAME: "postgresql://social:social_password@postgres:5432/social_api?sslmode=disable"
    ports:
      - "9187:9187"
    depends_on:
      postgres:
        condition: service_healthy
    healthcheck:
      test: ["CMD", "wget", "--quiet", "--tries=1", "--spider", "http://localhost:9187/metrics"]
      interval: 10s
      timeout: 3s
      retries: 3

  redis-exporter:
    image: oliver006/redis_exporter:v1.67.0
    environment:
      REDIS_ADDR: "redis://redis:6379"
    ports:
      - "9121:9121"
    depends_on:
      redis:
        condition: service_healthy
    healthcheck:
      test: ["CMD", "wget", "--quiet", "--tries=1", "--spider", "http://localhost:9121/metrics"]
      interval: 10s
      timeout: 3s
      retries: 3
```

Add to `volumes:` section:
```yaml
  prometheus_data:
  grafana_data:
```

**Step 2: Verify YAML syntax**

Run: `docker compose config --quiet`
Expected: No output (valid YAML).

**Step 3: Commit**

```
feat(monitoring): add Prometheus, Grafana, and exporters to docker-compose

Adds prometheus (v3, port 9090), grafana (11.5.2, port 3000, anonymous
admin), postgres-exporter (port 9187), and redis-exporter (v1.67.0,
port 9121). All services have healthchecks and proper dependency
ordering. Prometheus retains 7d of data.
```

---

### Task 7: Smoke test the full stack

**Step 1: Build and start all services**

Run: `docker compose up --build -d`
Expected: All 8 services start (postgres, redis, mock-services, social-api, prometheus, grafana, postgres-exporter, redis-exporter).

**Step 2: Wait for all services healthy**

Run: `docker compose ps`
Expected: All services show `(healthy)` status.

**Step 3: Verify Prometheus targets**

Run: `curl -s http://localhost:9090/api/v1/targets | python3 -m json.tool | grep -E '"health"|"job"'`
Expected: All 4 jobs (social-api, postgres-exporter, redis-exporter, prometheus) show `"health": "up"`.

**Step 4: Verify Grafana datasource**

Run: `curl -s http://localhost:3000/api/datasources | python3 -m json.tool | grep -E '"name"|"type"'`
Expected: Prometheus datasource listed.

**Step 5: Verify dashboard loaded**

Run: `curl -s http://localhost:3000/api/search?query=Social | python3 -m json.tool | grep title`
Expected: "Social API Overview" dashboard found.

**Step 6: Verify alert rules loaded**

Run: `curl -s http://localhost:3000/api/v1/provisioning/alert-rules | python3 -m json.tool | grep title`
Expected: All 12 alert rule titles listed.

**Step 7: Generate some traffic and verify metrics flow**

Run: `curl -s http://localhost:8080/v1/likes/post/00000000-0000-0000-0000-000000000001/count`
Wait 20 seconds for Prometheus scrape, then:
Run: `curl -s "http://localhost:9090/api/v1/query?query=social_api_http_requests_total" | python3 -m json.tool | grep '"__name__"'`
Expected: Metric data present.

**Step 8: Verify histogram buckets (not summaries)**

Run: `curl -s http://localhost:8080/metrics | grep social_api_http_request_duration_seconds_bucket | head -5`
Expected: Lines with `le="0.001"`, `le="0.005"` etc. (bucket format, not quantile format).

**Step 9: Commit (if any fixes were needed)**

No commit if everything passes. If fixes were needed during smoke test, commit them.

---

### Task Summary

| Task | Type | Effort |
|---|---|---|
| 1. Switch histograms to buckets | Rust (2 lines) | 2 min |
| 2. Prometheus config | YAML (1 file) | 2 min |
| 3. Grafana provisioning | YAML (2 files) | 3 min |
| 4. Alert rules | YAML (1 file) | 5 min |
| 5. Dashboard JSON | JSON (1 large file) | 15 min |
| 6. Docker Compose update | YAML (modify) | 5 min |
| 7. Smoke test | Manual verification | 5 min |

**Total estimated time:** ~35 minutes

**Dependency order:** Tasks 1-5 are independent and can run in parallel. Task 6 depends on 2, 3, 4, 5 (files must exist for volume mounts). Task 7 depends on all.
