//! The staff phone app end to end, against a real PostgreSQL: sign in with a bearer token, look a
//! pack up by its barcode, receive it with batch and expiry (twice with the same key: added once),
//! and see it in the expiring-soon list.
//! Each test gets its own throw-away database, so point it at a LOCAL server, never at Supabase:
//!   DATABASE_URL=postgres://USER:PASS@localhost:5432/postgres cargo test --test admin_app_db -- --ignored

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::*;
use serde_json::{Value, json};
use sqlx::PgPool;
use wagwell_api::services::{orders::today_ist, password};

const PASSWORD: &str = "a-long-enough-password";

fn app_post(path: &str, token: Option<&str>, key: Option<&str>, body: Value) -> Request<Body> {
    let mut req = Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-wagwell-client", "mobile");
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    if let Some(k) = key {
        req = req.header("idempotency-key", k);
    }
    req.body(Body::from(body.to_string())).unwrap()
}

fn app_get(path: &str, token: &str) -> Request<Body> {
    Request::get(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

async fn add_staff(pool: &PgPool, email: &str, role: &str) {
    sqlx::query(
        "INSERT INTO admin_users (email, name, role, password_hash) VALUES ($1, 'Asha', $2, $3)",
    )
    .bind(email)
    .bind(role)
    .bind(password::hash(PASSWORD).unwrap())
    .execute(pool)
    .await
    .unwrap();
}

/// The phone app's staff sign-in: the token comes back in the body, and no cookie is set.
async fn app_login(app: &Router, email: &str) -> String {
    let res = send(
        app,
        app_post(
            "/api/v1/admin/auth/login",
            None,
            None,
            json!({ "email": email, "password": PASSWORD }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        res.headers().get(header::SET_COOKIE).is_none(),
        "the app gets no cookie"
    );
    let body = json(res).await;
    assert_eq!(body["admin"]["email"], email);
    body["token"].as_str().expect("token in body").to_owned()
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn phone_sessions_last_a_shift_but_website_sessions_still_idle_out(pool: PgPool) {
    let app = router(pool.clone());
    add_staff(&pool, "stock@wagwell.test", "inventory_manager").await;
    let token = app_login(&app, "stock@wagwell.test").await;

    // An hour on the shop floor without touching the app is fine on the phone…
    sqlx::query("UPDATE admin_sessions SET last_seen_at = now() - interval '1 hour'")
        .execute(&pool)
        .await
        .unwrap();
    let res = send(&app, app_get("/api/v1/admin/auth/me", &token)).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(json(res).await["role"], "inventory_manager");

    // …but a website session that sat idle that long is over.
    let res = send(
        &app,
        post_json(
            "/api/v1/admin/auth/login",
            json!({ "email": "stock@wagwell.test", "password": PASSWORD }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = cookie_pair(&res);
    sqlx::query(
        "UPDATE admin_sessions SET last_seen_at = now() - interval '1 hour' WHERE client = 'web'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let req = with_cookie(
        Request::get("/api/v1/admin/auth/me")
            .body(Body::empty())
            .unwrap(),
        &cookie,
    );
    assert_eq!(send(&app, req).await.status(), StatusCode::UNAUTHORIZED);

    // Signing out on the phone ends the phone session.
    let res = send(
        &app,
        app_post("/api/v1/admin/auth/logout", Some(&token), None, json!({})),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let res = send(&app, app_get("/api/v1/admin/auth/me", &token)).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn scan_receive_and_expiry(pool: PgPool) {
    let app = router(pool.clone());
    add_staff(&pool, "stock@wagwell.test", "inventory_manager").await;
    let token = app_login(&app, "stock@wagwell.test").await;

    // Look the pack up by its barcode, and by its SKU.
    let res = send(&app, app_get("/api/v1/admin/barcodes/890123456701", &token)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let found = json(res).await;
    let hit = &found["matches"][0];
    assert_eq!(hit["variant"]["id"], "lra-1-2");
    assert_eq!(hit["product"]["id"], "lamb-rice-adult");
    let stock = hit["variant"]["stock"].as_i64().unwrap();
    let res = send(&app, app_get("/api/v1/admin/barcodes/wg-lra-1-2", &token)).await;
    assert_eq!(json(res).await["matches"][0]["variant"]["id"], "lra-1-2");

    // An unknown code is an empty list, not an error; junk is refused.
    let res = send(
        &app,
        app_get("/api/v1/admin/barcodes/0000000000000", &token),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(json(res).await["matches"], json!([]));
    let res = send(&app, app_get("/api/v1/admin/barcodes/a%20b", &token)).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // Receive 12 with a batch that expires in 30 days.
    let expiry = today_ist() + chrono::Days::new(30);
    let body = json!({
        "supplierRef": "INV-77",
        "lines": [{ "variantId": "lra-1-2", "qty": 12, "batchNo": "B-2610", "expiryDate": expiry }],
    });
    let res = send(
        &app,
        app_post(
            "/api/v1/admin/inventory/receipts",
            Some(&token),
            Some("scan-0001-abcdef"),
            body.clone(),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let first = json(res).await;
    assert_eq!(first["lines"][0]["before"].as_i64().unwrap(), stock);
    assert_eq!(first["lines"][0]["after"].as_i64().unwrap(), stock + 12);

    // The same tap again (lost answer, double tap): the stored result, stock added once.
    let res = send(
        &app,
        app_post(
            "/api/v1/admin/inventory/receipts",
            Some(&token),
            Some("scan-0001-abcdef"),
            body,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let again = json(res).await;
    assert_eq!(again["receiptId"], first["receiptId"]);
    assert_eq!(again["replayed"], true);
    let now: i32 = sqlx::query_scalar("SELECT stock FROM variants WHERE id = 'lra-1-2'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(i64::from(now), stock + 12);

    // The same key with different items is refused.
    let res = send(
        &app,
        app_post(
            "/api/v1/admin/inventory/receipts",
            Some(&token),
            Some("scan-0001-abcdef"),
            json!({ "lines": [{ "variantId": "lra-1-2", "qty": 1 }] }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // No key, expired stock, unknown sizes: refused, and nothing changes.
    let res = send(
        &app,
        app_post(
            "/api/v1/admin/inventory/receipts",
            Some(&token),
            None,
            json!({ "lines": [{ "variantId": "lra-1-2", "qty": 1 }] }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        app_post(
            "/api/v1/admin/inventory/receipts",
            Some(&token),
            Some("scan-0002-abcdef"),
            json!({ "lines": [{ "variantId": "lra-1-2", "qty": 1, "expiryDate": today_ist() - chrono::Days::new(1) }] }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        app_post(
            "/api/v1/admin/inventory/receipts",
            Some(&token),
            Some("scan-0003-abcdef"),
            json!({ "lines": [{ "variantId": "lra-1-2", "qty": 1 }, { "variantId": "nope", "qty": 1 }] }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let after: i32 = sqlx::query_scalar("SELECT stock FROM variants WHERE id = 'lra-1-2'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, now, "a refused goods-in changes nothing");

    // The batch is on the stock log and in the expiring-soon list.
    let (batch, exp): (Option<String>, Option<chrono::NaiveDate>) = sqlx::query_as(
        "SELECT batch_no, expiry_date FROM inventory_txns WHERE receipt_id IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(batch.as_deref(), Some("B-2610"));
    assert_eq!(exp, Some(expiry));
    let res = send(
        &app,
        app_get("/api/v1/admin/inventory/expiring?days=60", &token),
    )
    .await;
    let list = json(res).await;
    assert_eq!(list[0]["variantId"], "lra-1-2");
    assert_eq!(list[0]["batchNo"], "B-2610");
    let res = send(
        &app,
        app_get("/api/v1/admin/inventory/expiring?days=7", &token),
    )
    .await;
    assert_eq!(json(res).await, json!([]));

    // The stock ledger the admin website shows carries the batch too.
    let res = send(&app, app_get("/api/v1/admin/inventory/txns", &token)).await;
    let ledger = json(res).await;
    assert_eq!(ledger[0]["batchNo"], "B-2610");
    assert_eq!(ledger[0]["type"], "addition");

    // The lookup now remembers the last batch, to fill the next scan in.
    let res = send(&app, app_get("/api/v1/admin/barcodes/890123456701", &token)).await;
    assert_eq!(
        json(res).await["matches"][0]["lastBatch"]["batchNo"],
        "B-2610"
    );
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn support_staff_can_look_but_not_receive(pool: PgPool) {
    let app = router(pool.clone());
    add_staff(&pool, "help@wagwell.test", "support").await;
    let token = app_login(&app, "help@wagwell.test").await;
    let res = send(&app, app_get("/api/v1/admin/barcodes/890123456701", &token)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let res = send(
        &app,
        app_post(
            "/api/v1/admin/inventory/receipts",
            Some(&token),
            Some("scan-0009-abcdef"),
            json!({ "lines": [{ "variantId": "lra-1-2", "qty": 5 }] }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}
