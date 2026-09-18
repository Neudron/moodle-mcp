//! Retry, backoff, rate governor and circuit breaker (Task 2, P1).
//!
//! Policy (plan #1001–1050): at most [`MAX_RETRIES`] retries with
//! exponential backoff `500ms x 2^n` capped at 15s plus jitter; honor
//! `Retry-After`; retry 429 / 5xx / timeouts only. A token-bucket governor
//! (8 req/s, burst 16) and a per-host breaker (5 consecutive failures open
//! it for 30s) are shared by [`crate::moodle::Moodle`] API calls and file
//! downloads. Nothing here touches secret material.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

/// Default retry budget: initial attempt + this many retries.
pub const MAX_RETRIES: u32 = 4;
/// First backoff step; doubled per attempt.
pub const BACKOFF_BASE_MS: u64 = 500;
/// Backoff never exceeds this, before adding jitter.
pub const BACKOFF_CAP_MS: u64 = 15_000;
/// Upper bound of the random jitter added to each backoff sleep.
pub const BACKOFF_JITTER_MS: u64 = 250;

/// Default governor: sustained requests per second.
pub const DEFAULT_RPS: f64 = 8.0;
/// Default governor: burst allowance.
pub const DEFAULT_BURST: f64 = 16.0;
/// Breaker opens after this many consecutive qualifying failures.
pub const BREAKER_THRESHOLD: u32 = 5;
/// Breaker stays open for this long before half-opening.
pub const BREAKER_COOLDOWN_SECS: u64 = 30;

/// Backoff for retry `attempt` (0-based): `500ms x 2^attempt` capped at
/// 15s, plus up to [`BACKOFF_JITTER_MS`] of jitter sourced from the wall
/// clock (no extra dependency).
#[must_use]
pub fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.min(5);
    let grown = BACKOFF_BASE_MS.saturating_mul(1 << shift);
    let capped = grown.min(BACKOFF_CAP_MS);
    let jitter = system_jitter(BACKOFF_JITTER_MS + 1);
    Duration::from_millis(capped.saturating_add(jitter))
}

fn system_jitter(modulo: u64) -> u64 {
    if modulo == 0 {
        return 0;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()) % modulo)
        .unwrap_or(0)
}

/// Parse a `Retry-After` header value in delay-seconds form.
/// HTTP-date form is not honored (falls back to backoff); unparseable
/// values yield `None`.
#[must_use]
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// `Retry-After` delay from response headers, if present and parseable.
#[must_use]
pub fn retry_after_from_headers(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_retry_after)
}

/// Statuses worth retrying: 429 and 5xx. Auth/not-found/client errors and
/// transport failures that are not timeouts abort immediately.
#[must_use]
pub fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// `--offline` gate: when `MOODLE_OFFLINE` is `1`/`true`/`yes`, all network
/// paths must abort before connecting (plan #1028).
#[must_use]
pub fn offline_blocked() -> bool {
    matches!(
        std::env::var("MOODLE_OFFLINE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// Token-bucket governor: sustained `rps` with `burst` allowance.
/// Cloned handles share nothing; wrap in `Arc` to share one governor.
#[derive(Debug)]
pub struct RateLimiter {
    rps: f64,
    burst: f64,
    state: Mutex<LimiterState>,
}

#[derive(Debug)]
struct LimiterState {
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// Sustained `rps` with `burst` tokens of headroom. Non-positive inputs
    /// fall back to the plan defaults instead of deadlocking.
    #[must_use]
    pub fn with_rate(rps: f64, burst: f64) -> Self {
        let rps = if rps.is_finite() && rps > 0.0 {
            rps
        } else {
            DEFAULT_RPS
        };
        let burst = if burst.is_finite() && burst >= 1.0 {
            burst
        } else {
            DEFAULT_BURST
        };
        Self {
            rps,
            burst,
            state: Mutex::new(LimiterState {
                tokens: burst,
                last: Instant::now(),
            }),
        }
    }

    /// Plan-default governor: 8/s sustained, burst 16.
    #[must_use]
    pub fn default_governor() -> Self {
        Self::with_rate(DEFAULT_RPS, DEFAULT_BURST)
    }

    fn refill(state: &mut LimiterState, rps: f64, burst: f64) {
        let now = Instant::now();
        let elapsed = now.duration_since(state.last).as_secs_f64();
        if elapsed > 0.0 {
            state.tokens = (state.tokens + elapsed * rps).min(burst);
            state.last = now;
        }
    }

    /// Non-blocking acquire for tests and fast paths.
    pub fn try_acquire(&self) -> bool {
        let Ok(mut state) = self.state.try_lock() else {
            return false;
        };
        Self::refill(&mut state, self.rps, self.burst);
        if state.tokens >= 1.0 {
            state.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Wait until one token is available, then consume it.
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut state = self.state.lock().await;
                Self::refill(&mut state, self.rps, self.burst);
                if state.tokens >= 1.0 {
                    state.tokens -= 1.0;
                    return;
                }
                Duration::from_secs_f64((1.0 - state.tokens) / self.rps)
            };
            tokio::time::sleep(wait.max(Duration::from_millis(1))).await;
        }
    }
}

/// Per-host circuit breaker: after [`BREAKER_THRESHOLD`] consecutive
/// qualifying failures it opens for [`BREAKER_COOLDOWN_SECS`], failing fast
/// instead of hammering a sick server. Auth/not-found/invalid outcomes must
/// NOT feed it (callers decide); only transport/rate-limit/server failures
/// count. Best-effort under lock contention (fail-closed accounting, never
/// fail-open traffic): contended checks allow traffic.
#[derive(Debug)]
pub struct CircuitBreaker {
    threshold: u32,
    cooldown: Duration,
    state: Mutex<BreakerState>,
}

#[derive(Debug)]
struct BreakerState {
    consecutive_failures: u32,
    opened_at: Option<Instant>,
}

impl CircuitBreaker {
    /// Plan defaults: 5 failures, 30s cooldown.
    #[must_use]
    pub fn new() -> Self {
        Self::with_tuning(
            BREAKER_THRESHOLD,
            Duration::from_secs(BREAKER_COOLDOWN_SECS),
        )
    }

    /// Custom tuning (tests use short cooldowns).
    #[must_use]
    pub fn with_tuning(threshold: u32, cooldown: Duration) -> Self {
        Self {
            threshold: threshold.max(1),
            cooldown,
            state: Mutex::new(BreakerState {
                consecutive_failures: 0,
                opened_at: None,
            }),
        }
    }

    /// `true` while the breaker is open (caller must fail fast).
    /// A cooled-down breaker half-opens (resets) here.
    pub fn is_open(&self) -> bool {
        let Ok(mut state) = self.state.try_lock() else {
            return false;
        };
        match state.opened_at {
            Some(at) if at.elapsed() < self.cooldown => true,
            Some(_) => {
                state.consecutive_failures = 0;
                state.opened_at = None;
                false
            }
            None => false,
        }
    }

    /// A call fully succeeded: close the breaker.
    pub fn record_success(&self) {
        if let Ok(mut state) = self.state.try_lock() {
            state.consecutive_failures = 0;
            state.opened_at = None;
        }
    }

    /// A qualifying failure (transport/429/5xx after retries exhausted).
    pub fn record_failure(&self) {
        if let Ok(mut state) = self.state.try_lock() {
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            if state.consecutive_failures >= self.threshold {
                state.opened_at = Some(Instant::now());
            }
        }
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_backoff_grows_then_caps() {
        let d0 = backoff_delay(0);
        let d1 = backoff_delay(1);
        let d2 = backoff_delay(2);
        assert!(d0 >= Duration::from_millis(500) && d0 < Duration::from_millis(750));
        assert!(d1 >= Duration::from_millis(1000) && d1 < Duration::from_millis(1250));
        assert!(d2 >= Duration::from_millis(2000) && d2 < Duration::from_millis(2250));
        for attempt in [5, 10, 100] {
            let d = backoff_delay(attempt);
            assert!(d >= Duration::from_millis(15_000));
            assert!(d < Duration::from_millis(15_000 + BACKOFF_JITTER_MS + 1));
        }
    }

    #[test]
    fn net_retry_after_seconds_form() {
        assert_eq!(parse_retry_after("2"), Some(Duration::from_secs(2)));
        assert_eq!(parse_retry_after("  30 "), Some(Duration::from_secs(30)));
        assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after("soon"), None);
        assert_eq!(parse_retry_after(""), None);
    }

    #[test]
    fn net_retryable_status_set() {
        use reqwest::StatusCode as S;
        assert!(is_retryable_status(S::TOO_MANY_REQUESTS));
        for s in [500, 502, 503, 504] {
            assert!(is_retryable_status(S::from_u16(s).unwrap_or(S::OK)));
        }
        for s in [200, 206, 301, 400, 401, 403, 404, 416, 422] {
            assert!(!is_retryable_status(S::from_u16(s).unwrap_or(S::OK)));
        }
    }

    #[test]
    fn net_limiter_burst_then_refills() {
        let lim = RateLimiter::with_rate(20.0, 2.0);
        assert!(lim.try_acquire());
        assert!(lim.try_acquire());
        assert!(!lim.try_acquire(), "burst exhausted");
        std::thread::sleep(Duration::from_millis(120));
        assert!(lim.try_acquire(), "tokens must refill over time");
    }

    #[test]
    fn net_limiter_bad_tuning_falls_back_to_defaults() {
        let lim = RateLimiter::with_rate(0.0, -3.0);
        let mut ok = 0;
        for _ in 0..16 {
            if lim.try_acquire() {
                ok += 1;
            }
        }
        assert_eq!(ok, 16, "default burst is 16");
        assert!(!lim.try_acquire());
    }

    #[test]
    fn net_breaker_opens_and_half_opens() {
        let br = CircuitBreaker::with_tuning(3, Duration::from_millis(60));
        assert!(!br.is_open());
        br.record_failure();
        br.record_failure();
        assert!(!br.is_open());
        br.record_failure();
        assert!(br.is_open(), "threshold reached");
        std::thread::sleep(Duration::from_millis(80));
        assert!(!br.is_open(), "cooldown half-opens");
        br.record_failure();
        assert!(!br.is_open(), "single failure must not reopen");
    }

    #[test]
    fn net_breaker_success_resets() {
        let br = CircuitBreaker::with_tuning(2, Duration::from_secs(30));
        br.record_failure();
        br.record_success();
        br.record_failure();
        assert!(!br.is_open(), "success must reset the count");
    }

    #[tokio::test]
    async fn net_acquire_eventually_proceeds() {
        let lim = RateLimiter::with_rate(50.0, 1.0);
        tokio::time::timeout(Duration::from_secs(5), lim.acquire())
            .await
            .expect("first acquire immediate");
        tokio::time::timeout(Duration::from_secs(5), lim.acquire())
            .await
            .expect("second acquire after short refill wait");
    }
}
