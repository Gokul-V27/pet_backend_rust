//! The public catalogue cache against a real PostgreSQL: repeat requests are served from memory,
//! staff changes show up at once, only one request loads a missing answer, and personal answers
//! are never cached. Each test gets its own throw-away database, so point it at a LOCAL server:
//!   DATABASE_URL=postgres://USER:PASS@localhost:5432/postgres cargo test --test catalogue_cache_db -- --ignored

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode, header};
use common::*;
use serde_json::{Value, json};
use sqlx::PgPool;
use wagwell_api::services::{otp, password};

fn x_cache(res: &Response<Body>) -> Option<String> {
    res.headers()
        .get("x-cache")
        .map(|v| v.to_str().unwrap().to_owned())
}

fn cache_control(res: &Response<Body>) -> String {
    res.headers()
        .get(header::CACHE_CONTROL)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default()
}

async fn staff(app: &Router, pool: &PgPool) -> String {
    sqlx::query(
        "INSERT INTO admin_users (email, name, role, password_hash) VALUES ('boss@wagwell.test', 'Boss', 'super_admin', $1)",
    )
    .bind(password::hash("a-long-enough-password").unwrap())
    .execute(pool)
    .await
    .unwrap();
    let res = send(
        app,
        post_json(
            "/api/v1/admin/auth/login",
            json!({ "email": "boss@wagwell.test", "password": "a-long-enough-password" }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    cookie_pair(&res)
}

async fn shopper(app: &Router, pool: &PgPool) -> String {
    let mobile = "9876500001";
    sqlx::query("INSERT INTO otp_codes (mobile, code_hash, expires_at) VALUES ($1, $2, now() + interval '5 minutes')")
        .bind(mobile)
        .bind(otp::hash_code(PEPPER, mobile, "123456"))
        .execute(pool)
        .await
        .unwrap();
    let res = send(
        app,
        post_json("/api/v1/auth/otp/verify", json!({ "mobile": mobile, "code": "123456" })),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    cookie_pair(&res)
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn a_repeat_request_is_served_from_memory_even_without_the_database(pool: PgPool) {
    let app = router(pool.clone());
    let first = send(&app, get("/api/v1/categories")).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(x_cache(&first).as_deref(), Some("MISS"));
    assert!(cache_control(&first).starts_with("public, max-age=10"), "{}", cache_control(&first));
    let body = json(first).await;
    assert!(body.as_array().is_some_and(|a| !a.is_empty()));

    pool.close().await; // the database is gone; the cached copy must still answer
    let again = send(&app, get("/api/v1/categories")).await;
    assert_eq!(again.status(), StatusCode::OK);
    assert_eq!(x_cache(&again).as_deref(), Some("HIT"));
    assert_eq!(json(again).await, body);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn each_address_is_cached_separately(pool: PgPool) {
    let app = router(pool);
    let list = json(send(&app, get("/api/v1/products")).await).await;
    let slug = list[0]["slug"].as_str().unwrap().to_owned();
    let one = send(&app, get(&format!("/api/v1/products/{slug}"))).await;
    assert_eq!(x_cache(&one).as_deref(), Some("MISS"), "a product page is not the product list");
    assert_eq!(json(one).await["slug"], slug.as_str());
    let again = send(&app, get(&format!("/api/v1/products/{slug}"))).await;
    assert_eq!(x_cache(&again).as_deref(), Some("HIT"));
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn a_staff_change_shows_up_at_once(pool: PgPool) {
    let app = router(pool.clone());
    let boss = staff(&app, &pool).await;
    let label = |cats: &Value| {
        cats.as_array().unwrap().iter().find(|c| c["slug"] == "dry-food").unwrap()["label"].clone()
    };

    let before = json(send(&app, get("/api/v1/categories")).await).await;
    assert_ne!(label(&before), "Kibble");
    assert_eq!(x_cache(&send(&app, get("/api/v1/categories")).await).as_deref(), Some("HIT"));

    let edit = Request::builder()
        .method(Method::PUT)
        .uri("/api/v1/admin/categories/dry-food")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-wagwell-client", "web")
        .header(header::COOKIE, &boss)
        .body(Body::from(json!({ "slug": "dry-food", "label": "Kibble", "active": true, "sort": 1 }).to_string()))
        .unwrap();
    assert_eq!(send(&app, edit).await.status(), StatusCode::OK);

    let after = send(&app, get("/api/v1/categories")).await;
    assert_eq!(x_cache(&after).as_deref(), Some("MISS"), "the staff change must clear the cache");
    assert_eq!(label(&json(after).await), "Kibble");
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn a_refused_staff_change_keeps_the_cache(pool: PgPool) {
    let app = router(pool.clone());
    send(&app, get("/api/v1/categories")).await;
    // Signed out: the change is refused, so nothing changed and the cache stays.
    let edit = Request::builder()
        .method(Method::PUT)
        .uri("/api/v1/admin/categories/dry-food")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-wagwell-client", "web")
        .body(Body::from(json!({ "slug": "dry-food", "label": "Hacked" }).to_string()))
        .unwrap();
    assert_eq!(send(&app, edit).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(x_cache(&send(&app, get("/api/v1/categories")).await).as_deref(), Some("HIT"));
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn many_shoppers_at_once_cause_a_single_database_load(pool: PgPool) {
    let app = router(pool);
    let all = futures_join_all((0..50).map(|_| {
        let app = app.clone();
        async move { send(&app, get("/api/v1/products")).await }
    }))
    .await;
    assert!(all.iter().all(|r| r.status() == StatusCode::OK));
    let misses = all.iter().filter(|r| x_cache(r).as_deref() == Some("MISS")).count();
    assert_eq!(misses, 1, "only the first request should reach the database");
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn missing_products_are_not_cached(pool: PgPool) {
    let app = router(pool);
    for _ in 0..2 {
        let res = send(&app, get("/api/v1/products/no-such-product")).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(x_cache(&res), None);
        assert_eq!(cache_control(&res), "no-store");
    }
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn personal_answers_are_never_cached(pool: PgPool) {
    let app = router(pool.clone());
    let me = shopper(&app, &pool).await;
    for path in ["/api/v1/auth/me", "/api/v1/orders"] {
        for _ in 0..2 {
            let res = send(&app, with_cookie(get(path), &me)).await;
            assert_eq!(res.status(), StatusCode::OK, "{path}");
            assert_eq!(x_cache(&res), None, "{path}");
            assert_eq!(cache_control(&res), "no-store", "{path}");
        }
    }
    // Coupon checks are POSTs and depend on the input: never cached either.
    let res = send(&app, post_json("/api/v1/coupons/validate", json!({ "code": "NOPE" }))).await;
    assert_eq!(x_cache(&res), None);
}

/// Runs the futures concurrently (tokio is already a dependency; no extra crate needed).
async fn futures_join_all<F>(futs: impl Iterator<Item = F>) -> Vec<Response<Body>>
where
    F: std::future::Future<Output = Response<Body>> + Send + 'static,
{
    let handles: Vec<_> = futs.map(tokio::spawn).collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        out.push(h.await.unwrap());
    }
    out
}
