//! Per-visitor rate limits over HTTP, without a database: requests that are refused before any
//! query (bad input, missing signature) still count, so tiny limits can be tested quickly.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::*;
use serde_json::json;
use wagwell_api::{app, config::Config, state::AppState};

fn router_with(extra: &[(&'static str, &'static str)]) -> Router {
    let extra = extra.to_vec();
    let cfg = Config::from_lookup(move |k| {
        extra
            .iter()
            .find(|(key, _)| *key == k)
            .map(|(_, v)| (*v).to_owned())
            .or_else(|| match k {
                "APP_ENV" => Some("development".to_owned()),
                "DATABASE_URL" => Some("postgres://unused".to_owned()),
                "OTP_PEPPER" => Some(PEPPER.to_owned()),
                _ => None,
            })
    })
    .unwrap();
    app(AppState {
        db: lazy_pool(),
        cache: AppState::cache_from(&cfg),
        limiter: Default::default(),
        cfg: Arc::new(cfg),
        storage: None,
        razorpay: None,
        sms: None,
    })
}

/// A sign-in code request with a bad number: refused with 400 before the database.
fn bad_code_request(ip: &str) -> Request<Body> {
    let mut r = post_json("/api/v1/auth/otp/request", json!({ "mobile": "123" }));
    r.headers_mut()
        .insert("x-forwarded-for", ip.parse().unwrap());
    r
}

#[tokio::test]
async fn writes_over_the_limit_get_429_with_retry_after() {
    let app = router_with(&[("RATE_LIMIT_WRITES_PER_MINUTE", "2")]);
    for _ in 0..2 {
        assert_eq!(
            send(&app, bad_code_request("198.51.100.1")).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    let res = send(&app, bad_code_request("198.51.100.1")).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    let wait: u64 = res.headers()[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&wait), "{wait}");
    assert_eq!(json(res).await["error"]["code"], "rate_limited");
}

#[tokio::test]
async fn another_visitor_is_not_affected() {
    let app = router_with(&[("RATE_LIMIT_WRITES_PER_MINUTE", "1")]);
    send(&app, bad_code_request("198.51.100.1")).await;
    assert_eq!(
        send(&app, bad_code_request("198.51.100.1")).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        send(&app, bad_code_request("198.51.100.2")).await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn sign_in_codes_have_their_own_tighter_limit() {
    let app = router_with(&[]); // default 60 writes a minute, 10 code requests an hour
    for _ in 0..10 {
        assert_eq!(
            send(&app, bad_code_request("198.51.100.9")).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        send(&app, bad_code_request("198.51.100.9")).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn health_checks_and_payment_webhooks_are_never_limited() {
    let app = router_with(&[
        ("RATE_LIMIT_READS_PER_MINUTE", "1"),
        ("RATE_LIMIT_WRITES_PER_MINUTE", "1"),
    ]);
    for _ in 0..5 {
        assert_eq!(send(&app, get("/health")).await.status(), StatusCode::OK);
        let hook = Request::post("/api/v1/payments/razorpay/webhook")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        assert_ne!(
            send(&app, hook).await.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }
}

#[tokio::test]
async fn reads_have_their_own_limit() {
    let app = router_with(&[("RATE_LIMIT_READS_PER_MINUTE", "2")]);
    for _ in 0..2 {
        assert_eq!(
            send(&app, get("/api/v1/auth/me")).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        send(&app, get("/api/v1/auth/me")).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn limits_can_be_switched_off() {
    let app = router_with(&[("RATE_LIMIT", "off"), ("RATE_LIMIT_WRITES_PER_MINUTE", "1")]);
    for _ in 0..20 {
        assert_eq!(
            send(&app, bad_code_request("198.51.100.1")).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
}
