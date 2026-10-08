//! Online payments, refunds and the other money paths, against a real PostgreSQL. Each test gets
//! its own throw-away database (seeded catalogue included), so point it at a LOCAL server, never at
//! Supabase:
//!   DATABASE_URL=postgres://USER:PASS@localhost:5432/postgres cargo test --test payments_db -- --ignored
//!
//! Razorpay itself is never called for real: online orders are created straight through the library
//! with a fake gateway order id, and payments are confirmed with correctly signed requests, exactly
//! as Razorpay's browser callback and webhook would send them. (Refunds do try to reach Razorpay
//! with fake keys, which fails; that is what the "refund failed" tests rely on.)

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::*;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::PgPool;
use uuid::Uuid;
use wagwell_api::services::orders::{self, CartLine, QuoteRequest};
use wagwell_api::services::pricing::paise;
use wagwell_api::services::{otp, password};

const KEY_SECRET: &str = "key-secret";
const HOOK_SECRET: &str = "hook-secret";
const LRA: (&str, &str) = ("lamb-rice-adult", "lra-1-2"); // ₹949, 42 in stock

fn sign(secret: &str, msg: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(msg);
    hex::encode(mac.finalize().into_bytes())
}

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

async fn customer_id(pool: &PgPool, mobile: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM customers WHERE mobile = $1")
        .bind(mobile)
        .fetch_one(pool)
        .await
        .unwrap()
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

fn tomorrow() -> chrono::NaiveDate {
    orders::today_ist().succ_opt().unwrap()
}

fn address() -> Value {
    json!({ "name": "Priya", "phone": "9876543210", "line1": "12 Test St", "area": "Adyar", "city": "Chennai", "pincode": "600020" })
}

fn cod_body(qty: i64) -> Value {
    json!({
        "lines": [{ "productId": LRA.0, "variantId": LRA.1, "qty": qty }],
        "address": address(),
        "slot": { "date": tomorrow(), "label": "Tomorrow, 9 am – 1 pm", "kind": "standard" },
        "payment": "cod",
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

/// A pending online order with a fake gateway order, made the way `place` makes one.
async fn online_order(
    pool: &PgPool,
    customer: Uuid,
    qty: i64,
    coupon: Option<&str>,
    gateway_order: &str,
) -> (Uuid, String, i64) {
    let mut tx = pool.begin().await.unwrap();
    let req = QuoteRequest {
        lines: vec![CartLine {
            product_id: LRA.0.into(),
            variant_id: LRA.1.into(),
            qty,
            autoship: false,
            frequency_days: None,
        }],
        coupon_code: coupon.map(str::to_owned),
        delivery: Some("standard".into()),
        payment: Some("upi".into()),
        donate: false,
    };
    let (_, q) = orders::quote(&mut tx, Some(customer), &req, true)
        .await
        .unwrap();
    assert!(q.coupon_error.is_none(), "{:?}", q.coupon_error);
    let (id, number) = orders::insert_order(
        &mut tx,
        customer,
        &format!("key-{gateway_order}"),
        &q,
        "upi",
        "pending",
        "pending",
        &address(),
        tomorrow(),
        "Tomorrow, 9 am – 1 pm",
        "standard",
        false,
        "test-hash",
    )
    .await
    .unwrap();
    orders::take_stock(&mut tx, &q, &number).await.unwrap();
    orders::add_event(
        &mut tx,
        id,
        "pending",
        Some("Waiting for payment"),
        "customer",
        None,
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO payments (order_id, provider, method, amount_paise, status, gateway_order_id) VALUES ($1, 'razorpay', 'upi', $2, 'pending', $3)")
        .bind(id)
        .bind(paise(q.total))
        .bind(gateway_order)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (id, number, paise(q.total))
}

fn verify_request(cookie: &str, order: &str, payment: &str, signature: &str) -> Request<Body> {
    with_cookie(
        post_json(
            "/api/v1/payments/razorpay/verify",
            json!({ "razorpay_order_id": order, "razorpay_payment_id": payment, "razorpay_signature": signature }),
        ),
        cookie,
    )
}

fn webhook(event: &str, event_id: &str, body: Value, secret: &str) -> Request<Body> {
    let raw = body.to_string();
    let _ = event;
    Request::post("/api/v1/payments/razorpay/webhook")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-razorpay-signature", sign(secret, raw.as_bytes()))
        .header("x-razorpay-event-id", event_id)
        .body(Body::from(raw))
        .unwrap()
}

fn captured_event(order: &str, payment: &str, amount: i64) -> Value {
    json!({ "event": "payment.captured", "payload": { "payment": { "entity": { "id": payment, "order_id": order, "amount": amount } } } })
}

async fn order_row(pool: &PgPool, number: &str) -> (String, String) {
    sqlx::query_as("SELECT status, payment_status FROM orders WHERE number = $1")
        .bind(number)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn entries(pool: &PgPool, source_type: &str, purpose: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM journal_entries WHERE source_type = $1 AND purpose = $2",
    )
    .bind(source_type)
    .bind(purpose)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn trial_balance_is_balanced(app: &Router, boss: &str) {
    let tb = json(
        send(
            app,
            with_cookie(get("/api/v1/admin/finance/trial-balance"), boss),
        )
        .await,
    )
    .await;
    assert_eq!(tb["balanced"], true, "{tb}");
}

/* ───────────────────────── Confirming a payment ───────────────────────── */

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn browser_confirmation_pays_the_order_once(pool: PgPool) {
    let app = router_with_razorpay(pool.clone());
    let me = sign_in(&app, &pool, "9876543210").await;
    let (_, number, total) = online_order(
        &pool,
        customer_id(&pool, "9876543210").await,
        1,
        None,
        "order_a1",
    )
    .await;
    let good = sign(KEY_SECRET, b"order_a1|pay_a1");

    // Wrong signature, and someone else's valid signature, both refused.
    let bad = send(
        &app,
        verify_request(
            &me,
            "order_a1",
            "pay_a1",
            &sign("other", b"order_a1|pay_a1"),
        ),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json(bad).await["error"]["code"], "payment_not_verified");
    let other = sign_in(&app, &pool, "9123456789").await;
    let stolen = send(&app, verify_request(&other, "order_a1", "pay_a1", &good)).await;
    assert_eq!(stolen.status(), StatusCode::BAD_REQUEST);
    assert_eq!(order_row(&pool, &number).await.0, "pending");

    // The real thing confirms it and posts exactly one entry; a repeat changes nothing.
    for _ in 0..2 {
        let res = send(&app, verify_request(&me, "order_a1", "pay_a1", &good)).await;
        assert_eq!(res.status(), StatusCode::OK);
        let v = json(res).await;
        assert_eq!(
            (v["status"].as_str(), v["paymentStatus"].as_str()),
            (Some("confirmed"), Some("paid"))
        );
        assert_eq!(v["pricing"]["total"].as_i64(), Some(total / 100));
    }
    assert_eq!(entries(&pool, "payment", "captured").await, 1);
    let (dr, cr): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(debit_paise) FILTER (WHERE account_code = '1010'), 0)::bigint,
                COALESCE(SUM(credit_paise) FILTER (WHERE account_code = '2400'), 0)::bigint FROM journal_lines",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((dr, cr), (total, total));
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn webhook_confirms_once_and_refuses_forgeries(pool: PgPool) {
    let app = router_with_razorpay(pool.clone());
    sign_in(&app, &pool, "9876543210").await;
    let (_, number, total) = online_order(
        &pool,
        customer_id(&pool, "9876543210").await,
        1,
        None,
        "order_w1",
    )
    .await;

    // Bad signature: refused, nothing changes. Wrong amount: not captured.
    let forged = send(
        &app,
        webhook(
            "payment.captured",
            "evt-1",
            captured_event("order_w1", "pay_w1", total),
            "not-the-secret",
        ),
    )
    .await;
    assert_eq!(forged.status(), StatusCode::UNAUTHORIZED);
    let short = send(
        &app,
        webhook(
            "payment.captured",
            "evt-2",
            captured_event("order_w1", "pay_w1", total - 100),
            HOOK_SECRET,
        ),
    )
    .await;
    assert_eq!(short.status(), StatusCode::OK);
    assert_eq!(order_row(&pool, &number).await.0, "pending");

    // The real event confirms it; replays and a second event id for the same payment add nothing.
    for evt in ["evt-3", "evt-3", "evt-4"] {
        let res = send(
            &app,
            webhook(
                "payment.captured",
                evt,
                captured_event("order_w1", "pay_w1", total),
                HOOK_SECRET,
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK, "{evt}");
    }
    assert_eq!(
        order_row(&pool, &number).await,
        ("confirmed".into(), "paid".into())
    );
    assert_eq!(entries(&pool, "payment", "captured").await, 1);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn one_failed_attempt_does_not_close_the_checkout(pool: PgPool) {
    let app = router_with_razorpay(pool.clone());
    sign_in(&app, &pool, "9876543210").await;
    let before = stock(&pool, LRA.1).await;
    let (_, number, total) = online_order(
        &pool,
        customer_id(&pool, "9876543210").await,
        2,
        None,
        "order_f1",
    )
    .await;
    let failed = json!({ "event": "payment.failed", "payload": { "payment": { "entity": { "id": "pay_f1", "order_id": "order_f1" } } } });
    assert_eq!(
        send(
            &app,
            webhook("payment.failed", "evt-f1", failed, HOOK_SECRET)
        )
        .await
        .status(),
        StatusCode::OK
    );
    // Still waiting, stock still held, so the retry inside the same Razorpay window can succeed.
    assert_eq!(order_row(&pool, &number).await.0, "pending");
    assert_eq!(stock(&pool, LRA.1).await, before - 2);
    let res = send(
        &app,
        webhook(
            "payment.captured",
            "evt-f2",
            captured_event("order_f1", "pay_f2", total),
            HOOK_SECRET,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(order_row(&pool, &number).await.0, "confirmed");
}

/* ───────────────────────── Unpaid orders and late payments ───────────────────────── */

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn unpaid_orders_expire_and_give_everything_back(pool: PgPool) {
    let app = router_with_razorpay(pool.clone());
    sign_in(&app, &pool, "9876543210").await;
    let cust = customer_id(&pool, "9876543210").await;
    let before = stock(&pool, LRA.1).await;

    // Old with a coupon, old without any payment row at all, and a fresh one.
    let (old, old_no, _) = online_order(&pool, cust, 2, Some("FIRSTBOWL"), "order_e1").await;
    let (orphan, orphan_no, _) = online_order(&pool, cust, 1, None, "order_e2").await;
    sqlx::query("DELETE FROM payments WHERE order_id = $1")
        .bind(orphan)
        .execute(&pool)
        .await
        .unwrap();
    let (_, fresh_no, _) = online_order(&pool, cust, 1, None, "order_e3").await;
    sqlx::query("UPDATE orders SET created_at = now() - interval '31 minutes' WHERE id = ANY($1)")
        .bind(vec![old, orphan])
        .execute(&pool)
        .await
        .unwrap();
    let used = |pool: PgPool| async move {
        sqlx::query_scalar::<_, i32>("SELECT used FROM coupons WHERE code = 'FIRSTBOWL'")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert_eq!(used(pool.clone()).await, 1);

    assert_eq!(orders::expire_unpaid(&pool).await.unwrap(), 2);
    assert_eq!(order_row(&pool, &old_no).await.0, "payment-failed");
    assert_eq!(order_row(&pool, &orphan_no).await.0, "payment-failed");
    assert_eq!(order_row(&pool, &fresh_no).await.0, "pending");
    assert_eq!(
        stock(&pool, LRA.1).await,
        before - 1,
        "only the fresh order still holds stock"
    );
    assert_eq!(
        used(pool.clone()).await,
        0,
        "the abandoned checkout gave its coupon use back"
    );
    // Running it again finds nothing.
    assert_eq!(orders::expire_unpaid(&pool).await.unwrap(), 0);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn late_payment_after_timeout_revives_the_order(pool: PgPool) {
    let app = router_with_razorpay(pool.clone());
    sign_in(&app, &pool, "9876543210").await;
    let before = stock(&pool, LRA.1).await;
    let (id, number, total) = online_order(
        &pool,
        customer_id(&pool, "9876543210").await,
        2,
        Some("FIRSTBOWL"),
        "order_l1",
    )
    .await;
    sqlx::query("UPDATE orders SET created_at = now() - interval '31 minutes' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    orders::expire_unpaid(&pool).await.unwrap();
    assert_eq!(stock(&pool, LRA.1).await, before);

    let res = send(
        &app,
        webhook(
            "payment.captured",
            "evt-l1",
            captured_event("order_l1", "pay_l1", total),
            HOOK_SECRET,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        order_row(&pool, &number).await,
        ("confirmed".into(), "paid".into())
    );
    assert_eq!(
        stock(&pool, LRA.1).await,
        before - 2,
        "stock taken again, once"
    );
    let used: i32 = sqlx::query_scalar("SELECT used FROM coupons WHERE code = 'FIRSTBOWL'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(used, 1, "the coupon use came back with the order");
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn late_payment_when_the_stock_is_gone_is_still_recorded(pool: PgPool) {
    let app = router_with_razorpay(pool.clone());
    sign_in(&app, &pool, "9876543210").await;
    let boss = staff(&app, &pool, "boss@wagwell.example", "super_admin").await;
    sqlx::query("UPDATE variants SET stock = 1 WHERE id = $1")
        .bind(LRA.1)
        .execute(&pool)
        .await
        .unwrap();
    let (id, number, total) = online_order(
        &pool,
        customer_id(&pool, "9876543210").await,
        1,
        None,
        "order_s1",
    )
    .await;
    assert_eq!(stock(&pool, LRA.1).await, 0);
    sqlx::query("UPDATE orders SET created_at = now() - interval '31 minutes' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    orders::expire_unpaid(&pool).await.unwrap();
    assert_eq!(stock(&pool, LRA.1).await, 1);

    // Someone else buys the last unit; then the first customer's payment finally lands.
    let other = sign_in(&app, &pool, "9123456789").await;
    let bought = send(&app, place(&other, "other-customer-1", cod_body(1))).await;
    assert_eq!(bought.status(), StatusCode::OK);
    assert_eq!(stock(&pool, LRA.1).await, 0);

    let res = send(
        &app,
        webhook(
            "payment.captured",
            "evt-s1",
            captured_event("order_s1", "pay_s1", total),
            HOOK_SECRET,
        ),
    )
    .await;
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "the webhook must succeed even though the stock is gone"
    );
    // The money is on the books, the order is closed and flagged for refund, no stock was oversold.
    let (status, pay_status) = order_row(&pool, &number).await;
    assert_eq!(status, "cancelled");
    assert!(
        pay_status == "paid" || pay_status == "refunded",
        "{pay_status}"
    );
    assert_eq!(entries(&pool, "payment", "captured").await, 1);
    assert_eq!(stock(&pool, LRA.1).await, 0);
    trial_balance_is_balanced(&app, &boss).await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn payment_on_a_cancelled_order_is_marked_paid_for_refund(pool: PgPool) {
    let app = router_with_razorpay(pool.clone());
    let me = sign_in(&app, &pool, "9876543210").await;
    let before = stock(&pool, LRA.1).await;
    let (_, number, total) = online_order(
        &pool,
        customer_id(&pool, "9876543210").await,
        1,
        None,
        "order_c1",
    )
    .await;
    // The customer cancels the unpaid checkout; their UPI collect then completes anyway.
    let res = send(
        &app,
        with_cookie(
            post_json(&format!("/api/v1/orders/{number}/cancel"), json!({})),
            &me,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(stock(&pool, LRA.1).await, before);
    let res = send(
        &app,
        webhook(
            "payment.captured",
            "evt-c1",
            captured_event("order_c1", "pay_c1", total),
            HOOK_SECRET,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let (status, pay_status) = order_row(&pool, &number).await;
    assert_eq!(status, "cancelled");
    assert!(
        pay_status == "paid" || pay_status == "refunded",
        "staff must be able to see and refund it: {pay_status}"
    );
    assert_eq!(
        stock(&pool, LRA.1).await,
        before,
        "no stock taken for a cancelled order"
    );
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn a_new_checkout_replaces_the_old_unpaid_one(pool: PgPool) {
    let app = router_with_razorpay(pool.clone());
    let me = sign_in(&app, &pool, "9876543210").await;
    let before = stock(&pool, LRA.1).await;
    let (_, old_no, _) = online_order(
        &pool,
        customer_id(&pool, "9876543210").await,
        3,
        Some("FIRSTBOWL"),
        "order_r1",
    )
    .await;
    assert_eq!(stock(&pool, LRA.1).await, before - 3);
    // Switching to cash on delivery starts a new checkout: the abandoned UPI one must not hold
    // stock or make FIRSTBOWL look already used.
    let mut body = cod_body(2);
    body["couponCode"] = json!("FIRSTBOWL");
    let res = send(&app, place(&me, "replacement-key-1", body)).await;
    assert_eq!(res.status(), StatusCode::OK, "{}", json(res).await);
    assert_eq!(order_row(&pool, &old_no).await.0, "payment-failed");
    assert_eq!(stock(&pool, LRA.1).await, before - 2);
}

/* ───────────────────────── Idempotency ───────────────────────── */

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn the_same_key_always_means_the_same_order(pool: PgPool) {
    let app = router(pool.clone());
    let me = sign_in(&app, &pool, "9876543210").await;
    let before = stock(&pool, LRA.1).await;
    let first = json(send(&app, place(&me, "idem-key-0001", cod_body(1))).await).await;
    let again = json(send(&app, place(&me, "idem-key-0001", cod_body(1))).await).await;
    assert_eq!(first["id"], again["id"]);
    assert_eq!(stock(&pool, LRA.1).await, before - 1);

    // Same key, different basket or address: refused, not silently the old order.
    let res = send(&app, place(&me, "idem-key-0001", cod_body(2))).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json(res).await["error"]["code"], "idempotency_key_reused");
    let mut moved = cod_body(1);
    moved["address"]["line1"] = json!("99 Another Road");
    let res = send(&app, place(&me, "idem-key-0001", moved)).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // After a cancel, replaying that key says the checkout has ended instead of returning a dead order.
    let number = first["id"].as_str().unwrap();
    let res = send(
        &app,
        with_cookie(
            post_json(&format!("/api/v1/orders/{number}/cancel"), json!({})),
            &me,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let res = send(&app, place(&me, "idem-key-0001", cod_body(1))).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(json(res).await["error"]["code"], "order_closed");
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn two_taps_at_once_make_one_order(pool: PgPool) {
    let app = router(pool.clone());
    let me = sign_in(&app, &pool, "9876543210").await;
    let before = stock(&pool, LRA.1).await;
    let (a, b) = tokio::join!(
        send(&app, place(&me, "double-tap-key-1", cod_body(1))),
        send(&app, place(&me, "double-tap-key-1", cod_body(1)))
    );
    assert_eq!((a.status(), b.status()), (StatusCode::OK, StatusCode::OK));
    let (ja, jb) = (json(a).await, json(b).await);
    assert_eq!(ja["id"], jb["id"]);
    assert_eq!(stock(&pool, LRA.1).await, before - 1);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM orders")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

/* ───────────────────────── Placement rules ───────────────────────── */

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn only_offered_delivery_slots_and_autoship_items_are_accepted(pool: PgPool) {
    let app = router(pool.clone());
    let me = sign_in(&app, &pool, "9876543210").await;
    // Standard delivery today, or five days out, is not something the shop offers.
    for date in [
        orders::today_ist(),
        orders::today_ist() + chrono::Days::new(5),
    ] {
        let mut body = cod_body(1);
        body["slot"]["date"] = json!(date);
        let res = send(&app, place(&me, &format!("slot-key-{date}"), body)).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{date}");
    }
    // Same-day delivery on another day, or in the wrong window.
    let mut body = cod_body(1);
    body["slot"] = json!({ "date": tomorrow(), "label": "Today, 6–9 pm", "kind": "express" });
    assert_eq!(
        send(&app, place(&me, "slot-key-express", body))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );

    // Autoship on a product that isn't Autoship-eligible is refused.
    sqlx::query("UPDATE products SET autoship_eligible = false WHERE id = $1")
        .bind(LRA.0)
        .execute(&pool)
        .await
        .unwrap();
    let mut body = cod_body(1);
    body["lines"][0]["autoship"] = json!(true);
    body["lines"][0]["frequencyDays"] = json!(30);
    let res = send(&app, place(&me, "autoship-key-1", body)).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM orders")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn a_coupon_with_one_use_left_goes_to_one_order_only(pool: PgPool) {
    let app = router(pool.clone());
    sqlx::query("UPDATE coupons SET usage_limit = 1, per_customer_limit = NULL, first_order_only = false WHERE code = 'FLAT100'")
        .execute(&pool)
        .await
        .unwrap();
    let a = sign_in(&app, &pool, "9876543210").await;
    let b = sign_in(&app, &pool, "9123456789").await;
    let mut body = cod_body(2);
    body["couponCode"] = json!("FLAT100");
    let (ra, rb) = tokio::join!(
        send(&app, place(&a, "coupon-key-a1", body.clone())),
        send(&app, place(&b, "coupon-key-b1", body))
    );
    let mut codes = [ra.status(), rb.status()];
    codes.sort();
    assert_eq!(
        codes,
        [StatusCode::OK, StatusCode::BAD_REQUEST],
        "exactly one order gets the last use"
    );
    let used: i32 = sqlx::query_scalar("SELECT used FROM coupons WHERE code = 'FLAT100'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(used, 1);
}

/* ───────────────────────── Refunds ───────────────────────── */

async fn delivered_cod_order(
    app: &Router,
    pool: &PgPool,
    ops: &str,
    me: &str,
    key: &str,
    qty: i64,
) -> String {
    let order = json(send(app, place(me, key, cod_body(qty))).await).await;
    let number = order["id"].as_str().unwrap().to_owned();
    for status in ["packed", "shipped", "delivered"] {
        let res = send(
            app,
            with_cookie(
                post_json(
                    &format!("/api/v1/admin/orders/{number}/status"),
                    json!({ "status": status }),
                ),
                ops,
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK, "{status}");
    }
    let _ = pool;
    number
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn refunds_never_pay_out_more_than_was_paid(pool: PgPool) {
    let app = router(pool.clone());
    let me = sign_in(&app, &pool, "9876543210").await;
    let ops = staff(&app, &pool, "ops@wagwell.example", "order_manager").await;
    let boss = staff(&app, &pool, "boss@wagwell.example", "super_admin").await;
    let number = delivered_cod_order(&app, &pool, &ops, &me, "refund-order-1", 2).await; // ₹1,898 + free delivery + ₹29 COD

    // Two refunds of ₹1,000 at once on a ₹1,927 payment: only one can be reserved.
    let refund = |reason: &str| {
        with_cookie(
            post_json(
                &format!("/api/v1/admin/orders/{number}/refund"),
                json!({ "amount": 1000, "reason": reason }),
            ),
            &boss,
        )
    };
    let (a, b) = tokio::join!(send(&app, refund("first")), send(&app, refund("second")));
    let mut codes = [a.status(), b.status()];
    codes.sort();
    assert_eq!(codes, [StatusCode::OK, StatusCode::BAD_REQUEST]);
    let refunded: i64 =
        sqlx::query_scalar("SELECT refunded_paise FROM payments WHERE provider = 'cod'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(refunded, 100_000);
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM refunds WHERE status = 'pending'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pending, 0);
    // Absurd input is a clean refusal, not an overflow.
    let res = send(
        &app,
        with_cookie(
            post_json(
                &format!("/api/v1/admin/orders/{number}/refund"),
                json!({ "amount": i64::MAX / 10, "reason": "x" }),
            ),
            &boss,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    trial_balance_is_balanced(&app, &boss).await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn a_refund_before_delivery_needs_a_cancelled_order(pool: PgPool) {
    let app = router_with_razorpay(pool.clone());
    let boss = staff(&app, &pool, "boss@wagwell.example", "super_admin").await;
    sign_in(&app, &pool, "9876543210").await;
    let (_, number, total) = online_order(
        &pool,
        customer_id(&pool, "9876543210").await,
        1,
        None,
        "order_p1",
    )
    .await;
    send(
        &app,
        webhook(
            "payment.captured",
            "evt-p1",
            captured_event("order_p1", "pay_p1", total),
            HOOK_SECRET,
        ),
    )
    .await;
    assert_eq!(order_row(&pool, &number).await.0, "confirmed");
    // A confirmed, undelivered order can't be partly refunded and then delivered with full revenue.
    let res = send(
        &app,
        with_cookie(
            post_json(
                &format!("/api/v1/admin/orders/{number}/refund"),
                json!({ "amount": 200, "reason": "goodwill" }),
            ),
            &boss,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let refunds: i64 = sqlx::query_scalar("SELECT count(*) FROM refunds")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(refunds, 0);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn cancelling_a_paid_order_stands_even_if_the_refund_fails(pool: PgPool) {
    // No Razorpay keys here: the automatic refund cannot be sent, which is exactly the failure to survive.
    let app = router_with_razorpay(pool.clone());
    let me = sign_in(&app, &pool, "9876543210").await;
    let before = stock(&pool, LRA.1).await;
    let (_, number, total) = online_order(
        &pool,
        customer_id(&pool, "9876543210").await,
        1,
        None,
        "order_x1",
    )
    .await;
    send(
        &app,
        webhook(
            "payment.captured",
            "evt-x1",
            captured_event("order_x1", "pay_x1", total),
            HOOK_SECRET,
        ),
    )
    .await;
    let res = send(
        &app,
        with_cookie(
            post_json(&format!("/api/v1/orders/{number}/cancel"), json!({})),
            &me,
        ),
    )
    .await;
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "a refund problem must not turn a cancel into an error"
    );
    let (status, pay_status) = order_row(&pool, &number).await;
    assert_eq!(
        (status.as_str(), pay_status.as_str()),
        ("cancelled", "paid"),
        "still refundable by staff"
    );
    assert_eq!(stock(&pool, LRA.1).await, before);
    let note: i64 =
        sqlx::query_scalar("SELECT count(*) FROM order_events WHERE note ILIKE '%refund%'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(note >= 1, "staff are told the refund needs doing");
    let failed: i64 = sqlx::query_scalar("SELECT count(*) FROM refunds WHERE status = 'failed'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(failed, 1);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn a_failed_gateway_refund_is_reversed_in_the_books(pool: PgPool) {
    let app = router_with_razorpay(pool.clone());
    let boss = staff(&app, &pool, "boss@wagwell.example", "super_admin").await;
    let ops = staff(&app, &pool, "ops@wagwell.example", "order_manager").await;
    sign_in(&app, &pool, "9876543210").await;
    let (_, number, total) = online_order(
        &pool,
        customer_id(&pool, "9876543210").await,
        2,
        None,
        "order_g1",
    )
    .await;
    send(
        &app,
        webhook(
            "payment.captured",
            "evt-g1",
            captured_event("order_g1", "pay_g1", total),
            HOOK_SECRET,
        ),
    )
    .await;
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
    // A ₹500 refund that Razorpay accepted (booked) and later reports as failed.
    let payment_id: Uuid =
        sqlx::query_scalar("SELECT id FROM payments WHERE gateway_order_id = 'order_g1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let refund_id: Uuid = sqlx::query_scalar("INSERT INTO refunds (payment_id, amount_paise, reason, status, gateway_refund_id) VALUES ($1, 50000, 'test', 'pending', 'rfnd_g1') RETURNING id")
        .bind(payment_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    // Booked inside a transaction, as the real callers do: the balance check runs at commit.
    for _ in 0..2 {
        // The second pass is a replay and must change nothing.
        let mut tx = pool.begin().await.unwrap();
        orders::book_refund(&mut tx, refund_id, "test")
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    let (refunded, status): (i64, String) =
        sqlx::query_as("SELECT refunded_paise, status FROM payments WHERE id = $1")
            .bind(payment_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((refunded, status.as_str()), (50_000, "partially_refunded"));
    trial_balance_is_balanced(&app, &boss).await;

    let failed = json!({ "event": "refund.failed", "payload": { "refund": { "entity": { "id": "rfnd_g1", "payment_id": "pay_g1" } } } });
    assert_eq!(
        send(
            &app,
            webhook("refund.failed", "evt-g2", failed, HOOK_SECRET)
        )
        .await
        .status(),
        StatusCode::OK
    );
    let (refunded, status): (i64, String) =
        sqlx::query_as("SELECT refunded_paise, status FROM payments WHERE id = $1")
            .bind(payment_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (refunded, status.as_str()),
        (0, "captured"),
        "the money is still ours and can be refunded again"
    );
    assert_eq!(order_row(&pool, &number).await.1, "paid");
    assert_eq!(entries(&pool, "refund", "refund-failed").await, 1);
    trial_balance_is_balanced(&app, &boss).await;
    // The 2400/GST/returns accounts are back to what delivery left, i.e. nothing is stuck in 4500.
    let returns: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(debit_paise - credit_paise), 0)::bigint FROM journal_lines WHERE account_code = '4500'").fetch_one(&pool).await.unwrap();
    assert_eq!(returns, 0);
}

/* ───────────────────────── Books ───────────────────────── */

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn the_books_open_for_any_past_date_and_orders_carry_invoice_figures(pool: PgPool) {
    let app = router(pool.clone());
    let me = sign_in(&app, &pool, "9876543210").await;
    let boss = staff(&app, &pool, "boss@wagwell.example", "super_admin").await;
    // The trial balance takes just an as-of date, however long ago.
    let res = send(
        &app,
        with_cookie(
            get("/api/v1/admin/finance/trial-balance?to=2020-01-31"),
            &boss,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(json(res).await["balanced"], true);
    let res = send(
        &app,
        with_cookie(
            get("/api/v1/admin/finance/pnl?from=2020-01-01&to=2020-01-31"),
            &boss,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);

    // Orders carry what an invoice needs: HSN, GST rate, and each line's GST and discount share.
    let order = json(send(&app, place(&me, "invoice-key-001", cod_body(1))).await).await;
    let line = &order["lines"][0];
    assert!(line["hsn"].as_str().is_some());
    assert!(line["gstRatePct"].as_i64().unwrap() >= 0);
    assert!(line["gstPaise"].as_i64().unwrap() > 0);
    assert_eq!(line["discountPaise"], 0);
}
