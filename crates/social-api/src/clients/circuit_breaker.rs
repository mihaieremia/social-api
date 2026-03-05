use std::collections::VecDeque;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU32, Ordering};
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
    state: RwLock<CircuitState>,
    consecutive_failures: AtomicU32,
    consecutive_successes: AtomicU32,
    last_failure_time: RwLock<Option<Instant>>,
    /// Sliding window of recent call outcomes for failure rate calculation.
    call_window: RwLock<VecDeque<CallRecord>>,
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            state: RwLock::new(CircuitState::Closed),
            consecutive_failures: AtomicU32::new(0),
            consecutive_successes: AtomicU32::new(0),
            last_failure_time: RwLock::new(None),
            call_window: RwLock::new(VecDeque::new()),
        }
    }

    /// Check if the circuit allows a request.
    /// Returns true if the request should proceed, false if it should fail fast.
    pub fn allow_request(&self) -> bool {
        let state = *self.state.read().unwrap();

        match state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                // Check if recovery timeout has elapsed
                let should_try = self
                    .last_failure_time
                    .read()
                    .unwrap()
                    .map(|t| t.elapsed() >= self.config.recovery_timeout)
                    .unwrap_or(false);

                if should_try {
                    self.transition_to(CircuitState::HalfOpen);
                    true
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => true,
        }
    }

    /// Record a successful request.
    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::SeqCst);
        self.push_call(true);

        let state = *self.state.read().unwrap();
        if state == CircuitState::HalfOpen {
            let successes = self.consecutive_successes.fetch_add(1, Ordering::SeqCst) + 1;
            if successes >= self.config.success_threshold {
                self.transition_to(CircuitState::Closed);
            }
        }
    }

    /// Record a failed request.
    pub fn record_failure(&self) {
        self.consecutive_successes.store(0, Ordering::SeqCst);
        *self.last_failure_time.write().unwrap() = Some(Instant::now());
        self.push_call(false);

        let state = *self.state.read().unwrap();
        match state {
            CircuitState::Closed => {
                let failures = self.consecutive_failures.fetch_add(1, Ordering::SeqCst) + 1;
                // Trip on consecutive failures
                if failures >= self.config.failure_threshold {
                    self.transition_to(CircuitState::Open);
                    return;
                }
                // Trip on failure rate in sliding window
                if self.failure_rate_exceeded() {
                    self.transition_to(CircuitState::Open);
                }
            }
            CircuitState::HalfOpen => {
                // Any failure in half-open goes back to open
                self.transition_to(CircuitState::Open);
            }
            CircuitState::Open => {
                // Already open, just update failure count
                self.consecutive_failures.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// Get current state.
    #[cfg(test)]
    pub fn state(&self) -> CircuitState {
        *self.state.read().unwrap()
    }

    /// Push a call outcome into the sliding window, pruning expired entries.
    fn push_call(&self, success: bool) {
        let now = Instant::now();
        let mut window = self.call_window.write().unwrap();

        // Prune entries older than the rate window
        let cutoff = now - self.config.rate_window;
        while let Some(front) = window.front() {
            if front.timestamp < cutoff {
                window.pop_front();
            } else {
                break;
            }
        }

        window.push_back(CallRecord {
            timestamp: now,
            success,
        });
    }

    /// Check if the failure rate in the sliding window exceeds the threshold.
    fn failure_rate_exceeded(&self) -> bool {
        let window = self.call_window.read().unwrap();
        let total = window.len() as u32;

        // Don't trip on rate if we haven't seen enough calls
        if total < self.config.min_calls_for_rate {
            return false;
        }

        let failures = window.iter().filter(|r| !r.success).count() as f64;
        let rate = failures / total as f64;
        rate > self.config.failure_rate_threshold
    }

    fn transition_to(&self, new_state: CircuitState) {
        let old_state = {
            let mut state = self.state.write().unwrap();
            let old = *state;
            *state = new_state;
            old
        };

        if old_state != new_state {
            tracing::warn!(
                service = %self.config.service_name,
                from = ?old_state,
                to = ?new_state,
                "Circuit breaker state transition"
            );

            // Update metrics
            metrics::gauge!(
                "social_api_circuit_breaker_state",
                "service" => self.config.service_name.clone(),
            )
            .set(new_state.as_gauge_value());

            // Reset counters on transition
            match new_state {
                CircuitState::Closed => {
                    self.consecutive_failures.store(0, Ordering::SeqCst);
                    self.consecutive_successes.store(0, Ordering::SeqCst);
                    // Clear the window on close so stale failures don't re-trip
                    self.call_window.write().unwrap().clear();
                }
                CircuitState::HalfOpen => {
                    self.consecutive_successes.store(0, Ordering::SeqCst);
                }
                CircuitState::Open => {
                    self.consecutive_successes.store(0, Ordering::SeqCst);
                }
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
            failure_threshold: 100, // high so consecutive doesn't trip
            min_calls_for_rate: 10,
            failure_rate_threshold: 0.5,
            ..test_config()
        };
        let cb = CircuitBreaker::new(config);

        // 4 successes + 6 failures = 60% failure rate (>50%)
        for _ in 0..4 {
            cb.record_success();
        }
        for _ in 0..5 {
            cb.record_failure();
            assert_eq!(cb.state(), CircuitState::Closed); // not enough calls yet
        }
        // 10th call (failure) — now 6/10 = 60% > 50%
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
    }

    #[test]
    fn test_failure_rate_needs_minimum_calls() {
        let config = CircuitBreakerConfig {
            failure_threshold: 100, // high so consecutive doesn't trip
            min_calls_for_rate: 10,
            failure_rate_threshold: 0.5,
            ..test_config()
        };
        let cb = CircuitBreaker::new(config);

        // 5 failures, but only 5 calls — below min_calls_for_rate
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

        // Trip via consecutive, then recover
        for _ in 0..3 {
            cb.record_failure();
        }
        assert_eq!(cb.state(), CircuitState::Open);

        std::thread::sleep(Duration::from_millis(20));
        cb.allow_request(); // HalfOpen
        cb.record_success();
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);

        // Window should be cleared — old failures shouldn't cause rate trip
        let window = cb.call_window.read().unwrap();
        assert!(window.is_empty());
    }
}
