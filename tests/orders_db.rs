//! Orders, payments and the ledger end to end, against a real PostgreSQL. Each test gets its own
//! throw-away database (seeded catalogue included), so point it at a LOCAL server, never at Supabase:
//!   DATABASE_URL=postgres://USER:PASS@localhost:5432/postgres cargo test --test orders_db -- --ignored

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{Value, json};
use sqlx::PgPool;
use wagwell_api::services::{otp, password};

async fn sign_in(app: &Router, pool: &PgPool, mobile: &str) -> String {
    sqlx::query("INSERT INTO otp_codes (mobile, code_hash, expires_at) VALUES ($1, $2, now() + interval '5 minutes')")
        .bind(mobile)
        .bind(otp::hash_code(PEPPER, mobile, "123456"))
        .execute(pool)
        .await
        .unwrap();
    let res = send(
        app,
        post_json(
            "/api/v1/auth/otp/verify",
            json!({ "mobile": mobile, "code": "123456" }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    cookie_pair(&res)
}

async fn staff(app: &Router, pool: &PgPool, email: &str, role: &str) -> String {
    sqlx::query(
        "INSERT INTO admin_users (email, name, role, password_hash) VALUES ($1, 'Test', $2, $3)",
    )
    .bind(email)
    .bind(role)
    .bind(password::hash("a-long-enough-password").unwrap())
    .execute(pool)
    .await
    .unwrap();
    let res = send(
        app,
        post_json(
            "/api/v1/admin/auth/login",
            json!({ "email": email, "password": "a-long-enough-password" }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    cookie_pair(&res)
}

fn order_body(qty: i64, expected_total: Option<i64>) -> Value {
    json!({
        "lines": [{ "productId": "lamb-rice-adult", "variantId": "lra-1-2", "qty": qty }],
        "address": { "name": "Priya", "phone": "9876543210", "line1": "12 Test St", "area": "Adyar", "city": "Chennai", "pincode": "600020" },
        "slot": { "date": wagwell_api::services::orders::today_ist().succ_opt().unwrap(), "label": "Tomorrow, 9 am – 1 pm", "kind": "standard" },
        "payment": "cod",
        // FIRSTBOWL needs ₹999 of food, so only the two-bag orders use it.
        "couponCode": if qty >= 2 { Some("FIRSTBOWL") } else { None },
        "donate": true,
        "expectedTotal": expected_total,
    })
}

fn place(cookie: &str, key: &str, body: Value) -> Request<Body> {
    let mut req = with_cookie(post_json("/api/v1/orders", body), cookie);
    req.headers_mut()
        .insert("idempotency-key", key.parse().unwrap());
    req
}

async fn stock(pool: &PgPool, variant: &str) -> i32 {
    sqlx::query_scalar("SELECT stock FROM variants WHERE id = $1")
        .bind(variant)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn cash_on_delivery_order_from_cart_to_the_books(pool: PgPool) {
    let app = router(pool.clone());
    let me = sign_in(&app, &pool, "9876543210").await;
    let before = stock(&pool, "lra-1-2").await;

    // 2 × ₹949 = ₹1,898; FIRSTBOWL 10 % = ₹190 off; free delivery; COD ₹29; donation ₹5 → ₹1,742.
    let wrong = send(
        &app,
        place(&me, "checkout-attempt-1", order_body(2, Some(1000))),
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::CONFLICT);
    assert_eq!(json(wrong).await["error"]["code"], "price_changed");
    assert_eq!(
        stock(&pool, "lra-1-2").await,
        before,
        "a refused order takes no stock"
    );

    let res = send(
        &app,
        place(&me, "checkout-attempt-2", order_body(2, Some(1742))),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let order = json(res).await;
    let number = order["id"].as_str().unwrap().to_owned();
    assert_eq!(order["status"], "confirmed");
    assert_eq!(order["paymentStatus"], "cod-due");
    assert_eq!(order["pricing"]["total"], 1742);
    assert_eq!(order["pricing"]["couponDiscount"], 190);
    assert_eq!(stock(&pool, "lra-1-2").await, before - 2);

    // A double tap (same key) returns the same order and takes no more stock.
    let again = json(
        send(
            &app,
            place(&me, "checkout-attempt-2", order_body(2, Some(1742))),
        )
        .await,
    )
    .await;
    assert_eq!(again["id"], number.as_str());
    assert_eq!(stock(&pool, "lra-1-2").await, before - 2);

    // Someone else's order looks exactly like a missing one.
    let other = sign_in(&app, &pool, "9123456789").await;
    let peek = send(
        &app,
        with_cookie(get(&format!("/api/v1/orders/{number}")), &other),
    )
    .await;
    assert_eq!(peek.status(), StatusCode::NOT_FOUND);

    // Staff move it along to delivered.
    let ops = staff(&app, &pool, "ops@wagwell.example", "order_manager").await;
    for status in ["packed", "shipped", "delivered"] {
        let res = send(
            &app,
            with_cookie(
                post_json(
                    &format!("/api/v1/admin/orders/{number}/status"),
                    json!({ "status": status }),
                ),
                &ops,
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK, "{status}");
    }
    let done = json(
        send(
            &app,
            with_cookie(get(&format!("/api/v1/admin/orders/{number}")), &ops),
        )
        .await,
    )
    .await;
    assert_eq!(done["status"], "delivered");
    assert_eq!(
        done["paymentStatus"], "paid",
        "cash was collected on delivery"
    );

    // An order manager can't see the books or send money back.
    let res = send(
        &app,
        with_cookie(get("/api/v1/admin/finance/trial-balance"), &ops),
    )
    .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = send(
        &app,
        with_cookie(
            post_json(
                &format!("/api/v1/admin/orders/{number}/refund"),
                json!({ "reason": "test" }),
            ),
            &ops,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // The books: balanced, cash in hand = the order, GST payable = the order's GST.
    let boss = staff(&app, &pool, "boss@wagwell.example", "super_admin").await;
    let tb = json(
        send(
            &app,
            with_cookie(get("/api/v1/admin/finance/trial-balance"), &boss),
        )
        .await,
    )
    .await;
    assert_eq!(tb["balanced"], true);
    let account = |code: &str| {
        tb["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["code"] == code)
            .unwrap()
            .clone()
    };
    assert_eq!(account("1000")["debitPaise"], 174_200);
    let (gst,): (i64,) = sqlx::query_as("SELECT gst_paise FROM orders WHERE number = $1")
        .bind(&number)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(account("2100")["creditPaise"], gst);
    assert_eq!(
        account("2200")["creditPaise"],
        500,
        "the ₹5 donation is owed to the charity, not income"
    );

    let pnl = json(send(&app, with_cookie(get("/api/v1/admin/finance/pnl"), &boss)).await).await;
    assert!(pnl["incomePaise"].as_i64().unwrap() > 0);

    // A ₹100 refund after delivery is a sales return; the books still balance.
    let res = send(
        &app,
        with_cookie(
            post_json(
                &format!("/api/v1/admin/orders/{number}/refund"),
                json!({ "amount": 100, "reason": "Bag arrived torn" }),
            ),
            &boss,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(json(res).await["paymentStatus"], "partially-refunded");
    let tb = json(
        send(
            &app,
            with_cookie(get("/api/v1/admin/finance/trial-balance"), &boss),
        )
        .await,
    )
    .await;
    assert_eq!(tb["balanced"], true);
    let returns = tb["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["code"] == "4500")
        .unwrap()["debitPaise"]
        .as_i64()
        .unwrap();
    assert!(
        returns > 0 && returns < 10_000,
        "the return is booked net of its GST share"
    );

    // Every staff action is in the audit log.
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM audit_log WHERE target = $1")
        .bind(&number)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(n >= 4);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn cancelling_puts_the_stock_back_once(pool: PgPool) {
    let app = router(pool.clone());
    let me = sign_in(&app, &pool, "9876543210").await;
    let before = stock(&pool, "lra-1-2").await;
    let order = json(send(&app, place(&me, "cancel-test-1", order_body(1, None))).await).await;
    let number = order["id"].as_str().unwrap().to_owned();
    assert_eq!(stock(&pool, "lra-1-2").await, before - 1);

    let res = send(
        &app,
        with_cookie(
            post_json(&format!("/api/v1/orders/{number}/cancel"), json!({})),
            &me,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(json(res).await["status"], "cancelled");
    assert_eq!(stock(&pool, "lra-1-2").await, before);

    let twice = send(
        &app,
        with_cookie(
            post_json(&format!("/api/v1/orders/{number}/cancel"), json!({})),
            &me,
        ),
    )
    .await;
    assert_eq!(twice.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        stock(&pool, "lra-1-2").await,
        before,
        "stock is only returned once"
    );
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn out_of_stock_is_refused_and_nothing_is_saved(pool: PgPool) {
    let app = router(pool.clone());
    let me = sign_in(&app, &pool, "9876543210").await;
    sqlx::query("UPDATE variants SET stock = 1 WHERE id = 'lra-1-2'")
        .execute(&pool)
        .await
        .unwrap();
    let res = send(&app, place(&me, "oos-test-1", order_body(2, None))).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(json(res).await["error"]["code"], "out_of_stock");
    let (orders,): (i64,) = sqlx::query_as("SELECT count(*) FROM orders")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(orders, 0);
    assert_eq!(stock(&pool, "lra-1-2").await, 1);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn the_ledger_refuses_unbalanced_or_edited_entries(pool: PgPool) {
    // Unbalanced: fails when the transaction commits.
    let mut tx = pool.begin().await.unwrap();
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO journal_entries (memo, source_type, source_id, purpose) VALUES ('t', 'test', gen_random_uuid(), 't') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO journal_lines (entry_id, account_code, debit_paise) VALUES ($1, '1000', 100)",
    )
    .bind(id)
    .execute(&mut *tx)
    .await
    .unwrap();
    assert!(tx.commit().await.is_err());

    // Balanced: fine, but can't be changed or removed afterwards.
    let mut tx = pool.begin().await.unwrap();
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO journal_entries (memo, source_type, source_id, purpose) VALUES ('t', 'test', gen_random_uuid(), 't') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO journal_lines (entry_id, account_code, debit_paise) VALUES ($1, '1000', 100), ($1, '4000', 0)")
        .bind(id)
        .execute(&mut *tx)
        .await
        .expect_err("a line with neither debit nor credit is refused");
    tx.rollback().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO journal_entries (memo, source_type, source_id, purpose) VALUES ('t', 'test', gen_random_uuid(), 't') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO journal_lines (entry_id, account_code, debit_paise, credit_paise) VALUES ($1, '1000', 100, 0), ($1, '4000', 0, 100)")
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        sqlx::query("UPDATE journal_lines SET debit_paise = 1 WHERE entry_id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM journal_entries WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .is_err()
    );
}
