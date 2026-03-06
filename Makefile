# Social API — Makefile
# Run `make` or `make help` to see all available commands.

.DEFAULT_GOAL := help
.PHONY: help \
        build check fmt lint \
        test test-unit test-unit-fast test-integration test-all \
        k6-load k6-load-read k6-load-batch k6-load-write k6-load-mixed k6-load-parallel k6-load-sse \
        k6-stress k6-spike k6-soak k6-breakpoint k6-tune-macos \
        up down build-docker logs \
        db-reset migrate sqlx-prepare \
        coverage coverage-full \
        health metrics \
        setup-hooks

# ---------------------------------------------------------------------------
# Variables (override on CLI: make k6-stress TARGET_RPS=50000)
# ---------------------------------------------------------------------------

BASE_URL        ?= http://localhost:8080
TARGET_RPS      ?= 100000
STRESS_DURATION ?= 30m
COMPOSE         := docker compose
COMPOSE_TEST    := docker compose -f docker-compose.yml -f docker-compose.test.yml

# ---------------------------------------------------------------------------
# Help
# ---------------------------------------------------------------------------

help: ## Show this help message
	@echo ""
	@echo "  Social API — available commands"
	@echo ""
	@echo "  BUILD"
	@awk 'BEGIN{FS=":.*##"} /^(build|check|fmt|lint).*:.*##/{printf "    make %-28s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo ""
	@echo "  TESTS"
	@awk 'BEGIN{FS=":.*##"} /^test.*:.*##/{printf "    make %-28s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo ""
	@echo "  K6 — LOAD (sequential scenario suite)"
	@awk 'BEGIN{FS=":.*##"} /^k6-load.*:.*##/{printf "    make %-28s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo ""
	@echo "  K6 — STRESS STRATEGIES"
	@awk 'BEGIN{FS=":.*##"} /^k6-(stress|spike|soak|breakpoint).*:.*##/{printf "    make %-28s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo ""
	@echo "  DOCKER"
	@awk 'BEGIN{FS=":.*##"} /^(up|down|build-docker|logs).*:.*##/{printf "    make %-28s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo ""
	@echo "  DATABASE"
	@awk 'BEGIN{FS=":.*##"} /^(db-reset|migrate|sqlx-prepare).*:.*##/{printf "    make %-28s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo ""
	@echo "  COVERAGE"
	@awk 'BEGIN{FS=":.*##"} /^coverage.*:.*##/{printf "    make %-28s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo ""
	@echo "  OBSERVABILITY"
	@awk 'BEGIN{FS=":.*##"} /^(health|metrics).*:.*##/{printf "    make %-28s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo ""
	@echo "  SETUP"
	@awk 'BEGIN{FS=":.*##"} /^setup.*:.*##/{printf "    make %-28s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo ""
	@echo "  Overridable vars: BASE_URL=$(BASE_URL)  TARGET_RPS=$(TARGET_RPS)  STRESS_DURATION=$(STRESS_DURATION)"
	@echo ""

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

build: ## Compile all workspace crates (debug)
	cargo build --workspace

check: ## Cargo check (fast type-check, no binary)
	cargo check --workspace

fmt: ## Format code with rustfmt
	cargo fmt --all

lint: ## Run Clippy with deny-warnings
	cargo clippy --workspace --all-targets -- -D warnings

# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

test-unit: ## Run unit tests — real infra via testcontainers (requires Docker)
	cargo test --workspace --bins

test-unit-fast: ## Run only pure unit tests — no containers, no Docker needed
	cargo test --workspace --bins \
	  -- --skip services::like_service::tests \
	     --skip repositories::like_repository::tests \
	     --skip middleware::rate_limit::tests::test_sliding_window

test-integration: ## Run integration tests (requires full stack: make up)
	cargo test --workspace --test '*' -- --ignored --test-threads=1 \
	  --skip test_rate_limit_write_endpoint \
	  --skip test_circuit_breaker_trips_on_profile_api_failure

# Rate limit test requires service started with default limits (not docker-compose overrides):
#   RATE_LIMIT_WRITE_PER_MINUTE=30 cargo run  (then run this target)
test-integration-rate-limit: ## Rate-limit test (service must run with RATE_LIMIT_WRITE_PER_MINUTE=30)
	cargo test --workspace --test integration_test -- --ignored --test-threads=1 \
	  test_rate_limit_write_endpoint

# Circuit breaker test requires manually stopping mock-services:
#   docker compose stop mock-services  (then run this target, then docker compose start mock-services)
test-integration-circuit-breaker: ## Circuit breaker test (requires mock-services stopped)
	cargo test --workspace --test integration_test -- --ignored --test-threads=1 \
	  test_circuit_breaker_trips_on_profile_api_failure

test: test-unit-fast ## Alias for test-unit-fast (no Docker required)
test-all: test-unit test-integration ## Run all tests (unit + integration)

# ---------------------------------------------------------------------------
# K6 — Load test (sequential, individual scenarios)
# ---------------------------------------------------------------------------

k6-load: ## Run the full sequential load suite (all scenarios in order)
	k6 run -e BASE_URL=$(BASE_URL) k6/load_test.js

k6-load-read: ## Load: isolated read path (8k rps, 60s)
	k6 run -e BASE_URL=$(BASE_URL) -e K6_SCENARIO=read_path k6/load_test.js

k6-load-batch: ## Load: isolated batch counts (500 rps, 60s)
	k6 run -e BASE_URL=$(BASE_URL) -e K6_SCENARIO=batch_path k6/load_test.js

k6-load-write: ## Load: isolated write path like/unlike (500 rps, 60s)
	k6 run -e BASE_URL=$(BASE_URL) -e K6_SCENARIO=write_path k6/load_test.js

k6-load-mixed: ## Load: mixed 80/15/5 workload (2k rps, 120s)
	k6 run -e BASE_URL=$(BASE_URL) -e K6_SCENARIO=mixed k6/load_test.js

k6-load-parallel: ## Load: all scenarios running simultaneously
	k6 run -e BASE_URL=$(BASE_URL) -e K6_SCENARIO=parallel k6/load_test.js

k6-load-sse: ## Load: SSE connection stress (ramp 0→200 connections)
	k6 run -e BASE_URL=$(BASE_URL) -e K6_SCENARIO=sse_stress k6/load_test.js

# ---------------------------------------------------------------------------
# K6 — Stress strategies
# ---------------------------------------------------------------------------

k6-tune-macos: ## Tune macOS TCP/socket limits for high-RPS k6 tests (requires sudo)
	@echo "Tuning macOS for high-RPS load testing..."
	sudo sysctl -w net.inet.ip.portrange.first=1024
	sudo sysctl -w net.inet.ip.portrange.hifirst=1024
	sudo sysctl -w net.inet.tcp.msl=1000
	sudo sysctl -w kern.maxfiles=65536
	sudo sysctl -w kern.maxfilesperproc=65536
	ulimit -n 65536
	@echo ""
	@echo "  Done. Ephemeral port range expanded, TIME_WAIT reduced."
	@echo "  Run 'make k6-stress' now."
	@echo ""

k6-stress: ## Stress: ramp to TARGET_RPS (default 100k), sustain STRESS_DURATION (default 30m)
	k6 run \
	  -e BASE_URL=$(BASE_URL) \
	  -e K6_SCENARIO=stress \
	  -e STRESS_TARGET_RPS=$(TARGET_RPS) \
	  -e STRESS_DURATION=$(STRESS_DURATION) \
	  k6/stress_test.js

k6-spike: ## Spike: instant surge to 10x normal load then recover
	k6 run \
	  -e BASE_URL=$(BASE_URL) \
	  -e K6_SCENARIO=spike \
	  -e STRESS_TARGET_RPS=$(TARGET_RPS) \
	  k6/stress_test.js

k6-soak: ## Soak: sustained low-to-medium load for 2h (endurance / memory leaks)
	k6 run \
	  -e BASE_URL=$(BASE_URL) \
	  -e K6_SCENARIO=soak \
	  -e STRESS_TARGET_RPS=$(TARGET_RPS) \
	  k6/stress_test.js

k6-breakpoint: ## Breakpoint: ramp until the system breaks (find capacity ceiling)
	k6 run \
	  -e BASE_URL=$(BASE_URL) \
	  -e K6_SCENARIO=breakpoint \
	  -e STRESS_TARGET_RPS=$(TARGET_RPS) \
	  k6/stress_test.js

# ---------------------------------------------------------------------------
# Docker
# ---------------------------------------------------------------------------

up: ## Start all services (postgres, redis, mock-services, social-api, observability)
	$(COMPOSE) up -d

down: ## Stop and remove all containers
	$(COMPOSE) down

build-docker: ## Build Docker images (social-api + mock-services) without cache
	$(COMPOSE) build --no-cache

logs: ## Tail logs for social-api (Ctrl+C to stop)
	$(COMPOSE) logs -f social-api

# ---------------------------------------------------------------------------
# Database
# ---------------------------------------------------------------------------

db-reset: ## Drop + recreate DB schema (destroys data — local dev only)
	$(COMPOSE) exec postgres psql -U social -d social_api -c "DROP SCHEMA public CASCADE; CREATE SCHEMA public;"
	$(MAKE) migrate

migrate: ## Run pending SQLx migrations
	sqlx migrate run --database-url "postgres://social:social_password@localhost:5432/social_api"

sqlx-prepare: ## Regenerate sqlx-data.json for offline compile-time checks
	cargo sqlx prepare --workspace -- --all-targets

# ---------------------------------------------------------------------------
# Coverage
# ---------------------------------------------------------------------------

COVERAGE_DIR := coverage

COVERAGE_EXCLUDE := crates/mock-services|src/main\.rs|src/logging\.rs|src/openapi\.rs

# Internal: generate the markdown report from accumulated coverage data
define coverage_report
	@{ \
	  echo "# Coverage Report"; \
	  echo ""; \
	  echo "Generated: $$(date -u '+%Y-%m-%d %H:%M UTC')"; \
	  echo ""; \
	  cargo llvm-cov report --summary-only --ignore-filename-regex '$(COVERAGE_EXCLUDE)' 2>/dev/null \
	    | grep -E '^(Filename|--|[a-z]|TOTAL)' \
	    | awk '\
	        NR==1 { \
	          printf "| File | Lines | Line %% | Functions | Fn %% | Regions | Region %% |\n"; \
	          printf "|------|------:|-------:|----------:|------:|--------:|----------:|\n"; \
	          next \
	        } \
	        /^--/ { next } \
	        { printf "| `%s` | %s/%s | **%s** | %s/%s | %s | %s/%s | %s |\n", \
	            $$1, ($$8-$$9), $$8, $$10, ($$5-$$6), $$5, $$7, ($$2-$$3), $$2, $$4 } \
	      '; \
	} > $(COVERAGE_DIR)/coverage.md
	@echo ""
	@echo "  LCOV  → $(COVERAGE_DIR)/lcov.info"
	@echo "  Report→ $(COVERAGE_DIR)/coverage.md"
	@tail -1 $(COVERAGE_DIR)/coverage.md
endef

coverage: ## Coverage: unit + http tests only (Docker for testcontainers; no compose needed)
	@mkdir -p $(COVERAGE_DIR)
	cargo llvm-cov --workspace --all-targets \
	  --ignore-filename-regex '$(COVERAGE_EXCLUDE)' \
	  --lcov --output-path $(COVERAGE_DIR)/lcov.info \
	  -- --test-threads=4
	$(coverage_report)

coverage-full: ## Coverage: all tests including docker-compose integration tests (runs compose up/down)
	@mkdir -p $(COVERAGE_DIR)
	@echo "── Phase 1: unit + http tests (testcontainers) ──"
	cargo llvm-cov clean --workspace
	cargo llvm-cov test --workspace --all-targets --no-report \
	  --ignore-filename-regex '$(COVERAGE_EXCLUDE)' \
	  -- --test-threads=4
	@echo ""
	@echo "── Phase 2: docker-compose integration tests (test overrides) ──"
	$(COMPOSE_TEST) up --build -d
	@echo "Waiting for social-api to become healthy..."
	@for i in $$(seq 1 45); do \
	  if curl -sf http://localhost:8080/health/live > /dev/null 2>&1; then \
	    echo "  healthy after $${i}s"; \
	    break; \
	  fi; \
	  sleep 1; \
	  if [ $$i -eq 45 ]; then echo "  ERROR: social-api not healthy after 45s" && exit 1; fi; \
	done
	cargo llvm-cov test --no-report \
	  --ignore-filename-regex '$(COVERAGE_EXCLUDE)' \
	  -p social-api --test integration_test --test graceful_shutdown_test \
	  -- --ignored --test-threads=1 \
	     --skip test_rate_limit_write_endpoint \
	     --skip test_circuit_breaker_trips_on_profile_api_failure
	@echo ""
	@echo "── Phase 3: circuit breaker lifecycle test (stops/starts mock-services) ──"
	@echo "Restarting social-api with test overrides (fresh circuit breaker state)..."
	$(COMPOSE_TEST) up -d --force-recreate social-api
	@for i in $$(seq 1 45); do \
	  if curl -sf http://localhost:8080/health/live > /dev/null 2>&1; then \
	    echo "  healthy after $${i}s"; \
	    break; \
	  fi; \
	  sleep 1; \
	  if [ $$i -eq 45 ]; then echo "  ERROR: social-api not healthy after 45s" && exit 1; fi; \
	done
	cargo llvm-cov test --no-report \
	  --ignore-filename-regex '$(COVERAGE_EXCLUDE)' \
	  -p social-api --test integration_test \
	  -- --ignored --test-threads=1 \
	     test_circuit_breaker_trips_on_profile_api_failure
	$(COMPOSE_TEST) down -v
	@echo ""
	@echo "── Generating combined report ──"
	cargo llvm-cov report \
	  --ignore-filename-regex '$(COVERAGE_EXCLUDE)' \
	  --lcov --output-path $(COVERAGE_DIR)/lcov.info
	$(coverage_report)

# ---------------------------------------------------------------------------
# Observability
# ---------------------------------------------------------------------------

health: ## Check /health/ready endpoint
	curl -s $(BASE_URL)/health/ready | jq .

metrics: ## Dump raw Prometheus metrics
	curl -s $(BASE_URL)/metrics

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

setup-hooks: ## Install git pre-commit hook (fmt + clippy gate)
	cp scripts/pre-commit .git/hooks/pre-commit
	chmod +x .git/hooks/pre-commit
	@echo "Pre-commit hook installed. Commits will be gated by cargo fmt + clippy."
