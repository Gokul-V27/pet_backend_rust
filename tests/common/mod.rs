#![allow(dead_code)]

use std::sync::Arc;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, Response, header},
};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use wagwell_api::{app, config::Config, state::AppState};

pub const PEPPER: &str = "0123456789abcdef0123456789abcdef";

pub fn config() -> Config {
    let get = |k: &str| match k {
        "APP_ENV" => Some("development".to_owned()),
        "DATABASE_URL" => Some("postgres://unused".to_owned()),
        "OTP_PEPPER" => Some(PEPPER.to_owned()),
        "OTP_DEV_ECHO" => Some("true".to_owned()),
        "CORS_ORIGINS" => Some("http://localhost:3000,http://localhost:3001".to_owned()),
        _ => None,
    };
    Config::from_lookup(get).unwrap()
}

pub fn router(pool: PgPool) -> Router {
    app(AppState {
        db: pool,
        cfg: Arc::new(config()),
        storage: None,
        razorpay: None,
        sms: None,
    })
}

/// The same app with Razorpay test keys configured (webhook secret: `hook-secret`).
pub fn router_with_razorpay(pool: PgPool) -> Router {
    let rzp = wagwell_api::services::razorpay::Razorpay::new(wagwell_api::config::RazorpayConfig {
        key_id: "rzp_test_x".to_owned(),
        key_secret: "key-secret".to_owned(),
        webhook_secret: "hook-secret".to_owned(),
    })
    .unwrap();
    app(AppState {
        db: pool,
        cfg: Arc::new(config()),
        storage: None,
        razorpay: Some(rzp),
        sms: None,
    })
}

/// A pool that never connects unless a query is actually run, for tests that stop before the database.
pub fn lazy_pool() -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
        .unwrap()
}

pub fn post_json(path: &str, body: Value) -> Request<Body> {
    Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-wagwell-client", "web")
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub fn with_cookie(mut req: Request<Body>, cookie: &str) -> Request<Body> {
    req.headers_mut()
        .insert(header::COOKIE, cookie.parse().unwrap());
    req
}

pub fn get(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).unwrap()
}

pub async fn send(app: &Router, req: Request<Body>) -> Response<Body> {
    app.clone().oneshot(req).await.unwrap()
}

pub async fn json(res: Response<Body>) -> Value {
    let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// `name=value` from the first Set-Cookie header, ready to send back as a Cookie header.
pub fn cookie_pair(res: &Response<Body>) -> String {
    let raw = res
        .headers()
        .get(header::SET_COOKIE)
        .expect("a Set-Cookie header")
        .to_str()
        .unwrap();
    raw.split(';').next().unwrap().to_owned()
}
