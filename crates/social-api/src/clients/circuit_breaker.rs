use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Circuit breaker states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation — requests pass through
    Closed,
    /// Testing recovery — limited requests allowed
    HalfOpen,
    /// Failing fast — requests rejected immediately
    Open,
}

impl CircuitState {
    pub fn as_gauge_value(&self) -> f64 {
        match self {
            Self::Closed => 0.0,
            Self::HalfOpen => 1.0,
            Self::Open => 2.0,
        }
    }
}

/// Configuration for circuit breaker behavior.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Number of consecutive failures to trip the breaker
    pub failure_threshold: u32,
    /// Time to wait before transitioning from Open to HalfOpen
    pub recovery_timeout: Duration,
    /// Number of consecutive successes in HalfOpen to close the breaker
    pub success_threshold: u32,
    /// Service name for logging and metrics
    pub service_name: String,
    /// Sliding window duration for failure rate calculation
    pub rate_window: Duration,
    /// Failure rate threshold (0.0-1.0) to trip the breaker
    pub failure_rate_threshold: f64,
    /// Minimum calls in the window before rate-based tripping applies
    pub min_calls_for_rate: u32,
}

/// A single call outcome in the sliding window.
#[derive(Debug, Clone, Copy)]
struct CallRecord {
    timestamp: Instant,
    success: bool,
}

/// All mutable circuit breaker state under a single lock.
///
/// Using one `Mutex<Inner>` instead of multiple separate `RwLock`/`Atomic`
/// primitives eliminates TOCTOU races where threads could interleave reads
/// and writes across separate locks.
struct Inner {
    state: CircuitState,
    consecutive_failures: u32,
    consecutive_successes: u32,
    last_failure_time: Option<Instant>,
    /// Sliding window of recent call outcomes for failure rate calculation.
    call_window: VecDeque<CallRecord>,
}

/// Thread-safe circuit breaker state machine.
///
/// State transitions:
/// - Closed -> Open: after `failure_threshold` consecutive failures
///   **or** >50% failure rate in a 30-second window (configurable)
/// - Open -> HalfOpen: after `recovery_timeout` elapses
/// - HalfOpen -> Closed: after `success_threshold` consecutive successes
/// - HalfOpen -> Open: on any failure
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    inner: Mutex<Inner>,
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(Inner {
                state: CircuitState::Closed,
                consecutive_failures: 0,
                consecutive_successes: 0,
                last_failure_time: None,
                call_window: VecDeque::new(),
            }),
        }
    }

    /// Check if the circuit allows a request.
    ///
    /// The entire check-and-transition is performed under one lock hold,
    /// eliminating the TOCTOU race that existed when state was read under a
    /// read lock and then transitioned under a separate write lock.
    pub fn allow_request(&self) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match inner.state {
            CircuitState::Closed | CircuitState::HalfOpen => true,
            CircuitState::Open => {
                let elapsed = inner
                    .last_failure_time
                    .map(|t| t.elapsed() >= self.config.recovery_timeout)
                    .unwrap_or(false);
                if elapsed {
                    Self::do_transition(&self.config, &mut inner, CircuitState::HalfOpen);
                }
                elapsed
            }
        }
    }

    /// Record a successful request.
    pub fn record_success(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.consecutive_failures = 0;

        // No point tracking outcomes we're already rejecting.
        if inner.state != CircuitState::Open {
            Self::push_call(&self.config, &mut inner, true);
        }

        if inner.state == CircuitState::HalfOpen {
            inner.consecutive_successes += 1;
            if inner.consecutive_successes >= self.config.success_threshold {
                Self::do_transition(&self.config, &mut inner, CircuitState::Closed);
            }
        }
    }

    /// Record a failed request.
    pub fn record_failure(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.consecutive_successes = 0;
        inner.last_failure_time = Some(Instant::now());

        // No point tracking outcomes we're already rejecting.
        if inner.state != CircuitState::Open {
            Self::push_call(&self.config, &mut inner, false);
        }

        match inner.state {
            CircuitState::Closed => {
                inner.consecutive_failures += 1;
                if inner.consecutive_failures >= self.config.failure_threshold {
                    Self::do_transition(&self.config, &mut inner, CircuitState::Open);
                    return;
                }
                if Self::failure_rate_exceeded(&self.config, &inner) {
                    Self::do_transition(&self.config, &mut inner, CircuitState::Open);
                }
            }
            CircuitState::HalfOpen => {
                // Any failure in half-open goes back to open immediately.
                Self::do_transition(&self.config, &mut inner, CircuitState::Open);
            }
            CircuitState::Open => {
                // Already open — just keep counting.
                inner.consecutive_failures += 1;
            }
        }
    }

    /// Get current state (test-only).
    #[cfg(test)]
    pub fn state(&self) -> CircuitState {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).state
    }

    /// Push a call outcome into the sliding window, pruning expired entries.
    fn push_call(config: &CircuitBreakerConfig, inner: &mut Inner, success: bool) {
        let now = Instant::now();
        let cutoff = now - config.rate_window;
        while let Some(front) = inner.call_window.front() {
            if front.timestamp < cutoff {
                inner.call_window.pop_front();
            } else {
                break;
            }
        }
        inner.call_window.push_back(CallRecord {
            timestamp: now,
            success,
        });
    }

    /// Check if the failure rate in the sliding window exceeds the threshold.
    fn failure_rate_exceeded(config: &CircuitBreakerConfig, inner: &Inner) -> bool {
        let total = inner.call_window.len() as u32;
        if total < config.min_calls_for_rate {
            return false;
        }
        let failures = inner.call_window.iter().filter(|r| !r.success).count() as f64;
        failures / total as f64 > config.failure_rate_threshold
    }

    fn do_transition(config: &CircuitBreakerConfig, inner: &mut Inner, new_state: CircuitState) {
        let old_state = inner.state;
        if old_state == new_state {
            return;
        }

        inner.state = new_state;

        tracing::warn!(
            service = %config.service_name,
            from = ?old_state,
            to = ?new_state,
            "Circuit breaker state transition"
        );

        metrics::gauge!(
            "social_api_circuit_breaker_state",
            "service" => config.service_name.clone(),
        )
        .set(new_state.as_gauge_value());

        match new_state {
            CircuitState::Closed => {
                inner.consecutive_failures = 0;
                inner.consecutive_successes = 0;
                // Clear window on close so stale failures don't re-trip via rate check.
                inner.call_window.clear();
            }
            CircuitState::HalfOpen => {
                inner.consecutive_successes = 0;
            }
            CircuitState::Open => {
                inner.consecutive_successes = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: 3,
            recovery_timeout: Duration::from_millis(100),
            success_threshold: 2,
            service_name: "test".to_string(),
            rate_window: Duration::from_secs(30),
            failure_rate_threshold: 0.5,
            min_calls_for_rate: 10,
        }
    }

    #[test]
    fn test_closed_allows_requests() {
        let cb = CircuitBreaker::new(test_config());
        assert!(cb.allow_request());
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_opens_after_threshold_failures() {
        let cb = CircuitBreaker::new(test_config());
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Closed);
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
    }

    #[test]
    fn test_open_rejects_requests() {
        let cb = CircuitBreaker::new(test_config());
        for _ in 0..3 {
            cb.record_failure();
        }
        assert!(!cb.allow_request());
    }

    #[test]
    fn test_half_open_after_recovery_timeout() {
        let config = CircuitBreakerConfig {
            recovery_timeout: Duration::from_millis(10),
            ..test_config()
        };
        let cb = CircuitBreaker::new(config);
        for _ in 0..3 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);

        std::thread::sleep(Duration::from_millis(20));
        assert!(cb.allow_request());
        assert_eq!(cb.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn test_closes_after_success_threshold() {
        let config = CircuitBreakerConfig {
            recovery_timeout: Duration::from_millis(10),
            ..test_config()
        };
        let cb = CircuitBreaker::new(config);
        for _ in 0..3 {
            cb.record_failure();
        }
        std::thread::sleep(Duration::from_millis(20));
        cb.allow_request(); // Transition to HalfOpen

        cb.record_success();
        assert_eq!(cb.state(), CircuitState::HalfOpen);
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_half_open_failure_reopens() {
        let config = CircuitBreakerConfig {
            recovery_timeout: Duration::from_millis(10),
            ..test_config()
        };
        let cb = CircuitBreaker::new(config);
        for _ in 0..3 {
            cb.record_failure();
        }
        std::thread::sleep(Duration::from_millis(20));
        cb.allow_request(); // Transition to HalfOpen

        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
    }

    #[test]
    fn test_success_resets_failure_count() {
        let cb = CircuitBreaker::new(test_config());
        cb.record_failure();
        cb.record_failure();
        cb.record_success(); // Resets counter
        cb.record_failure();
        cb.record_failure();
        // Should still be closed (only 2 consecutive failures)
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_failure_rate_trips_breaker() {
        let config = CircuitBreakerConfig {
            failure_threshold: 100,
            min_calls_for_rate: 10,
            failure_rate_threshold: 0.5,
            ..test_config()
        };
        let cb = CircuitBreaker::new(config);

        for _ in 0..4 {
            cb.record_success();
        }
        for _ in 0..5 {
            cb.record_failure();
            assert_eq!(cb.state(), CircuitState::Closed);
        }
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
    }

    #[test]
    fn test_failure_rate_needs_minimum_calls() {
        let config = CircuitBreakerConfig {
            failure_threshold: 100,
            min_calls_for_rate: 10,
            failure_rate_threshold: 0.5,
            ..test_config()
        };
        let cb = CircuitBreaker::new(config);

        for _ in 0..5 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_window_cleared_on_close() {
        let config = CircuitBreakerConfig {
            failure_threshold: 3,
            recovery_timeout: Duration::from_millis(10),
            min_calls_for_rate: 10,
            ..test_config()
        };
        let cb = CircuitBreaker::new(config);

        for _ in 0..3 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);

        std::thread::sleep(Duration::from_millis(20));
        cb.allow_request();
        cb.record_success();
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);

        let inner = cb.inner.lock().unwrap();
        assert!(inner.call_window.is_empty());
    }

    #[test]
    fn test_allow_request_atomic_no_double_transition() {
        use std::sync::Arc;
        let config = CircuitBreakerConfig {
            recovery_timeout: Duration::from_millis(1),
            ..test_config()
        };
        let cb = Arc::new(CircuitBreaker::new(config));
        for _ in 0..3 {
            cb.record_failure();
        }
        std::thread::sleep(Duration::from_millis(5));

        let handles: Vec<_> = (0..10)
            .map(|_| {
                let cb = Arc::clone(&cb);
                std::thread::spawn(move || cb.allow_request())
            })
            .collect();

        let allowed: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(allowed.iter().all(|&a| a));
        assert_eq!(cb.state(), CircuitState::HalfOpen);
    }
}
