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
    /// Service name for logging
    pub service_name: String,
}

/// Thread-safe circuit breaker state machine.
///
/// State transitions:
/// - Closed -> Open: after `failure_threshold` consecutive failures
/// - Open -> HalfOpen: after `recovery_timeout` elapses
/// - HalfOpen -> Closed: after `success_threshold` consecutive successes
/// - HalfOpen -> Open: on any failure
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    state: RwLock<CircuitState>,
    consecutive_failures: AtomicU32,
    consecutive_successes: AtomicU32,
    last_failure_time: RwLock<Option<Instant>>,
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            state: RwLock::new(CircuitState::Closed),
            consecutive_failures: AtomicU32::new(0),
            consecutive_successes: AtomicU32::new(0),
            last_failure_time: RwLock::new(None),
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

        let state = *self.state.read().unwrap();
        match state {
            CircuitState::Closed => {
                let failures = self.consecutive_failures.fetch_add(1, Ordering::SeqCst) + 1;
                if failures >= self.config.failure_threshold {
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

    /// Get current state (used in tests).
    #[cfg(test)]
    pub fn state(&self) -> CircuitState {
        *self.state.read().unwrap()
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
}
