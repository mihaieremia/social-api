# Circuit Breaker Fixes Plan

## Gaps Found

### 1. Missing >50% failure rate window (Spec line 297)
**Spec:** "After 5 consecutive failures **or** >50% failure rate in a 30-second window"
**Current:** Only consecutive failures tracked.
**Fix:** Add a sliding window of recent call results (success/failure with timestamps). On each `record_failure`, check if failure rate in last 30s exceeds 50%. Trip breaker if so.

### 2. External call metrics never recorded (Spec lines 330-331)
**Spec requires:**
- `social_api_external_calls_total{service, method, status}` -- counter
- `social_api_external_call_duration_seconds{service, method}` -- histogram
**Current:** Described in `init_metrics()` but never emitted.
**Fix:** Add timing + counter instrumentation to `HttpContentValidator::validate()` and `HttpTokenValidator::validate()`.

### 3. No circuit breaker integration test (Spec line 530)
**Spec:** "Circuit breaker triggering and recovery" in integration tests.
**Current:** Unit tests cover state machine. No integration test.
**Fix:** Add integration test that triggers breaker by sending requests with invalid auth to trip profile breaker, then verify `DEPENDENCY_UNAVAILABLE` 503 response, then recover.

## Implementation Order

1. **circuit_breaker.rs** -- Add failure rate window tracking
2. **content_client.rs** -- Add external call metrics
3. **profile_client.rs** -- Add external call metrics
4. **integration_test.rs** -- Add circuit breaker integration test

## Non-goals
- k6 already covers read/write/batch/mixed scenarios. Circuit breaker is a fault-injection test, not a load test -- integration test is the right place.
