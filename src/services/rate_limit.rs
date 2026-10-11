//! Per-visitor request limits (token buckets kept in memory), on top of the existing per-number
//! sign-in code limits and the staff lock-out. Each visitor (client IP) gets a bucket per rule;
//! a request takes one token; tokens refill steadily. An empty bucket answers 429 with Retry-After.
//!
//! The client IP is the first address in `X-Forwarded-For` (set by Render, Cloudflare and Vercel's
//! proxy), else the connection's own address. With several server instances each keeps its own
//! counts; move this to Redis or the database if that ever matters.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderValue, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::error::AppError;
use crate::state::AppState;

/// One limit: up to `burst` requests at once, refilling to `per_minute` a minute.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rule {
    pub name: &'static str,
    pub burst: f64,
    pub per_minute: f64,
}

impl Rule {
    const fn new(name: &'static str, burst: f64, per_minute: f64) -> Self {
        Self { name, burst, per_minute }
    }
}

/// The limits in force. Reads and writes are configurable; sensitive routes are fixed.
#[derive(Debug, Clone)]
pub struct Limits {
    pub reads: Rule,
    pub writes: Rule,
}

/// Sign-in code requests: 10 an hour per visitor (on top of 5 an hour per mobile number).
pub const OTP_REQUEST: Rule = Rule::new("otp-request", 10.0, 10.0 / 60.0);
/// Sign-in code checks: 30 an hour per visitor (on top of 5 guesses per code).
pub const OTP_VERIFY: Rule = Rule::new("otp-verify", 30.0, 30.0 / 60.0);
/// Staff password sign-in: 20 an hour per visitor (on top of the 5-try account lock).
pub const STAFF_LOGIN: Rule = Rule::new("staff-login", 20.0, 20.0 / 60.0);
/// Coupon checks: 30 a minute, so codes can't be guessed quickly.
pub const COUPON_CHECK: Rule = Rule::new("coupon-check", 30.0, 30.0);

impl Limits {
    pub fn new(reads_per_minute: u32, writes_per_minute: u32) -> Self {
        Self {
            reads: Rule::new("reads", f64::from(reads_per_minute), f64::from(reads_per_minute)),
            writes: Rule::new("writes", f64::from(writes_per_minute), f64::from(writes_per_minute)),
        }
    }

    /// The rules a request counts against: the general one plus any for its route.
    fn rules_for(&self, method: &Method, path: &str) -> Vec<Rule> {
        let general = if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
            self.reads
        } else {
            self.writes
        };
        let special = match path {
            "/api/v1/auth/otp/request" => Some(OTP_REQUEST),
            "/api/v1/auth/otp/verify" => Some(OTP_VERIFY),
            "/api/v1/admin/auth/login" => Some(STAFF_LOGIN),
            "/api/v1/coupons/validate" => Some(COUPON_CHECK),
            _ => None,
        };
        std::iter::once(general).chain(special).collect()
    }
}

struct Bucket {
    tokens: f64,
    updated: Instant,
}

/// How many buckets before idle ones are swept out.
const SWEEP_AT: usize = 50_000;

#[derive(Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<(&'static str, String), Bucket>>,
}

impl RateLimiter {
    /// Takes one token from each rule's bucket. `Err(wait)` if any is empty (nothing is taken then).
    pub fn check(&self, client: &str, rules: &[Rule], now: Instant) -> Result<(), Duration> {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if buckets.len() >= SWEEP_AT {
            // A bucket that has been idle long enough to be full again carries no information.
            buckets.retain(|_, b| now.duration_since(b.updated) < Duration::from_secs(3600));
        }
        let mut wait = Duration::ZERO;
        for r in rules {
            let b = buckets
                .entry((r.name, client.to_owned()))
                .or_insert(Bucket { tokens: r.burst, updated: now });
            let elapsed = now.duration_since(b.updated).as_secs_f64();
            b.tokens = (b.tokens + elapsed * r.per_minute / 60.0).min(r.burst);
            b.updated = now;
            if b.tokens < 1.0 {
                let secs = (1.0 - b.tokens) * 60.0 / r.per_minute;
                wait = wait.max(Duration::from_secs_f64(secs.max(1.0)));
            }
        }
        if !wait.is_zero() {
            return Err(wait);
        }
        for r in rules {
            if let Some(b) = buckets.get_mut(&(r.name, client.to_owned())) {
                b.tokens -= 1.0;
            }
        }
        Ok(())
    }
}

/// The visitor's address: first `X-Forwarded-For` entry, else the TCP peer, else "unknown".
pub fn client_ip(req: &Request) -> String {
    req.headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|ip| !ip.is_empty() && ip.len() <= 45)
        .map(str::to_owned)
        .or_else(|| {
            req.extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip().to_string())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Middleware for the whole API. Health checks and Razorpay webhooks are never limited.
pub async fn limit_requests(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let Some(limits) = &s.cfg.rate_limits else {
        return next.run(req).await;
    };
    let path = req.uri().path();
    if path == "/health" || path == "/ready" || path.starts_with("/api/v1/payments/razorpay/webhook") {
        return next.run(req).await;
    }
    let rules = limits.rules_for(req.method(), path);
    let client = client_ip(&req);
    match s.limiter.check(&client, &rules, Instant::now()) {
        Ok(()) => next.run(req).await,
        Err(wait) => {
            let secs = wait.as_secs().max(1);
            let mut res = AppError::RateLimited(format!(
                "too many requests; try again in {secs} seconds"
            ))
            .into_response();
            res.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(secs));
            res
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: Rule = Rule::new("t", 3.0, 60.0); // 3 at once, then one a second

    #[test]
    fn a_burst_passes_then_the_next_request_waits() {
        let l = RateLimiter::default();
        let t = Instant::now();
        for _ in 0..3 {
            assert!(l.check("1.1.1.1", &[R], t).is_ok());
        }
        let wait = l.check("1.1.1.1", &[R], t).unwrap_err();
        assert_eq!(wait, Duration::from_secs(1));
    }

    #[test]
    fn tokens_come_back_over_time() {
        let l = RateLimiter::default();
        let t = Instant::now();
        for _ in 0..3 {
            l.check("ip", &[R], t).unwrap();
        }
        assert!(l.check("ip", &[R], t).is_err());
        assert!(l.check("ip", &[R], t + Duration::from_millis(1100)).is_ok());
    }

    #[test]
    fn visitors_do_not_share_buckets() {
        let l = RateLimiter::default();
        let t = Instant::now();
        for _ in 0..3 {
            l.check("a", &[R], t).unwrap();
        }
        assert!(l.check("a", &[R], t).is_err());
        assert!(l.check("b", &[R], t).is_ok());
    }

    #[test]
    fn a_refused_request_takes_nothing_from_the_other_buckets() {
        let l = RateLimiter::default();
        let t = Instant::now();
        let tight = Rule::new("tight", 1.0, 1.0);
        let wide = Rule::new("wide", 2.0, 60.0);
        l.check("ip", &[tight, wide], t).unwrap();
        assert!(l.check("ip", &[tight, wide], t).is_err());
        // "wide" still has its second token because the refused request didn't take it.
        assert!(l.check("ip", &[wide], t).is_ok());
    }

    #[test]
    fn sensitive_routes_add_their_own_rule() {
        let lim = Limits::new(240, 30);
        let names = |m: Method, p: &str| lim.rules_for(&m, p).iter().map(|r| r.name).collect::<Vec<_>>();
        assert_eq!(names(Method::GET, "/api/v1/products"), ["reads"]);
        assert_eq!(names(Method::POST, "/api/v1/orders"), ["writes"]);
        assert_eq!(names(Method::POST, "/api/v1/auth/otp/request"), ["writes", "otp-request"]);
        assert_eq!(names(Method::POST, "/api/v1/admin/auth/login"), ["writes", "staff-login"]);
        assert_eq!(names(Method::POST, "/api/v1/coupons/validate"), ["writes", "coupon-check"]);
    }

    #[test]
    fn client_ip_prefers_the_forwarded_address() {
        let req = Request::builder()
            .header("x-forwarded-for", "203.0.113.7, 10.0.0.1")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(client_ip(&req), "203.0.113.7");
        let bare = Request::builder().body(axum::body::Body::empty()).unwrap();
        assert_eq!(client_ip(&bare), "unknown");
    }
}
