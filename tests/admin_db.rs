//! The staff side against a real PostgreSQL: who may change what, the input checks on those changes,
//! and the dashboard, reports and customer screens reading real orders. Each test gets its own
//! throw-away database, so point it at a LOCAL server, never at Supabase:
//!   DATABASE_URL=postgres://USER:PASS@localhost:5432/postgres cargo test --test admin_db -- --ignored

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use common::*;
use serde_json::{Value, json};
use sqlx::PgPool;
use wagwell_api::services::orders::today_ist;
use wagwell_api::services::{otp, password};
use wagwell_api::{app, config::Config, state::AppState};

const LRA: (&str, &str) = ("lamb-rice-adult", "lra-1-2"); // ₹949, 42 in stock

fn req(method: Method, path: &str, body: Value, cookie: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-wagwell-client", "web")
        .header(header::COOKIE, cookie)
        .body(Body::from(body.to_string()))
        .unwrap()
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

async fn cod_order(app: &Router, me: &str, key: &str, qty: i64) -> String {
    let body = json!({
        "lines": [{ "productId": LRA.0, "variantId": LRA.1, "qty": qty }],
        "address": { "name": "Priya", "phone": "9876543210", "line1": "12 Test St", "area": "Adyar", "city": "Chennai", "pincode": "600020" },
        "slot": { "date": today_ist().succ_opt().unwrap(), "label": "Tomorrow, 9 am – 1 pm", "kind": "standard" },
        "payment": "cod",
    });
    let mut r = with_cookie(post_json("/api/v1/orders", body), me);
    r.headers_mut()
        .insert("idempotency-key", key.parse().unwrap());
    let res = send(app, r).await;
    assert_eq!(res.status(), StatusCode::OK);
    json(res).await["id"].as_str().unwrap().to_owned()
}

/* ───────────────────────── Who may change what ───────────────────────── */

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn each_role_can_change_only_its_own_screens(pool: PgPool) {
    let app = router(pool.clone());
    let boss = staff(&app, &pool, "boss@wagwell.example", "super_admin").await;
    let support = staff(&app, &pool, "help@wagwell.example", "support").await;
    let stock = staff(&app, &pool, "stock@wagwell.example", "inventory_manager").await;
    let orders = staff(&app, &pool, "ops@wagwell.example", "order_manager").await;

    let product = json(
        send(
            &app,
            with_cookie(get(&format!("/api/v1/admin/products/{}", LRA.0)), &boss),
        )
        .await,
    )
    .await;
    let mut cheaper = product.clone();
    cheaper["variants"][0]["price"] = json!(1);
    let coupon = json!({ "code": "STAFF10", "title": "Staff test", "description": "Ten off", "kind": "percent", "value": 10, "minOrder": 0, "firstOrderOnly": false, "excludesAutoshipLines": false, "used": 0, "active": true });
    let adjust = json!({ "productId": LRA.0, "variantId": LRA.1, "qty": 5, "type": "addition", "reason": "Delivery from supplier" });

    // Prices, coupons and settings: only Super Admin and Admin.
    for who in [&support, &stock, &orders] {
        let res = send(
            &app,
            req(
                Method::PUT,
                &format!("/api/v1/admin/products/{}", LRA.0),
                cheaper.clone(),
                who,
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = send(
            &app,
            req(Method::POST, "/api/v1/admin/coupons", coupon.clone(), who),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = send(
            &app,
            req(
                Method::DELETE,
                "/api/v1/admin/coupons/FIRSTBOWL",
                json!({}),
                who,
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = send(
            &app,
            req(
                Method::PUT,
                "/api/v1/admin/settings",
                json!({ "urgentDays": 3 }),
                who,
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = send(&app, with_cookie(get("/api/v1/admin/audit"), who)).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }
    let price: i32 = sqlx::query_scalar("SELECT price FROM variants WHERE id = $1")
        .bind(LRA.1)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(price, 949, "no price changed");
    let coupons: i64 =
        sqlx::query_scalar("SELECT count(*) FROM coupons WHERE code IN ('STAFF10', 'FIRSTBOWL')")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(coupons, 1, "nothing created or deleted");

    // Stock: the inventory manager yes, support and order manager no.
    for who in [&support, &orders] {
        let res = send(
            &app,
            req(
                Method::POST,
                "/api/v1/admin/inventory/adjust",
                adjust.clone(),
                who,
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }
    let res = send(
        &app,
        req(
            Method::POST,
            "/api/v1/admin/inventory/adjust",
            adjust.clone(),
            &stock,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(json(res).await["after"], 47);

    // Message templates: support writes them.
    let tmpl = json!({ "name": "order_placed", "event": "Order placed", "channel": "whatsapp", "body": "Hi {{name}}", "approval": "draft", "active": true });
    let res = send(
        &app,
        req(
            Method::PUT,
            "/api/v1/admin/messages/templates/t-test",
            tmpl.clone(),
            &support,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let res = send(
        &app,
        req(
            Method::PUT,
            "/api/v1/admin/messages/templates/t-test",
            tmpl,
            &stock,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // The boss can do all of it, and the audit log says who did what.
    let res = send(
        &app,
        req(
            Method::PUT,
            &format!("/api/v1/admin/products/{}", LRA.0),
            product,
            &boss,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let res = send(
        &app,
        req(Method::POST, "/api/v1/admin/coupons", coupon, &boss),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let log = json(send(&app, with_cookie(get("/api/v1/admin/audit"), &boss)).await).await;
    let actions: Vec<&str> = log
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["action"].as_str())
        .collect();
    assert!(
        actions
            .iter()
            .any(|a| a.contains("save_coupon") && a.contains("boss@wagwell.example")),
        "{actions:?}"
    );
    assert!(
        actions
            .iter()
            .any(|a| a.contains("save_msg_template") && a.contains("help@wagwell.example")),
        "{actions:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn bad_changes_are_refused_cleanly(pool: PgPool) {
    let app = router(pool.clone());
    let boss = staff(&app, &pool, "boss@wagwell.example", "super_admin").await;
    let adjust = |qty: i64, kind: &str| json!({ "productId": LRA.0, "variantId": LRA.1, "qty": qty, "type": kind, "reason": "x" });
    for body in [
        adjust(0, "addition"),
        adjust(2_000_000, "addition"),
        adjust(5, "gift"),
    ] {
        let res = send(
            &app,
            req(
                Method::POST,
                "/api/v1/admin/inventory/adjust",
                body.clone(),
                &boss,
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{body}");
    }
    // A write-off bigger than the stock stops at zero, never below.
    let res = send(
        &app,
        req(
            Method::POST,
            "/api/v1/admin/inventory/adjust",
            adjust(-500, "adjustment"),
            &boss,
        ),
    )
    .await;
    assert_eq!(json(res).await["after"], 0);

    let res = send(
        &app,
        req(
            Method::PUT,
            "/api/v1/admin/settings",
            json!({ "anything": true }),
            &boss,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        req(
            Method::PUT,
            "/api/v1/admin/settings",
            json!({ "urgentDays": 5 }),
            &boss,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);

    let review: Option<String> = sqlx::query_scalar("SELECT id FROM reviews LIMIT 1")
        .fetch_optional(&pool)
        .await
        .unwrap();
    if let Some(id) = review {
        let res = send(
            &app,
            req(
                Method::PUT,
                &format!("/api/v1/admin/reviews/{id}"),
                json!({ "status": "deleted" }),
                &boss,
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    // Saving the sample campaign never resets how many have been claimed.
    let mut camp = json(
        send(
            &app,
            with_cookie(get("/api/v1/admin/samples/campaign"), &boss),
        )
        .await,
    )
    .await;
    if camp.get("id").is_none() {
        camp = json!({ "id": "first-bowl", "name": "First bowl", "productId": LRA.0, "size": "200 g", "startsAt": "2026-01-01", "endsAt": "2026-12-31", "maxClaims": 100, "perHousehold": 1, "deliveryFee": 0, "firstOrderOnly": true, "active": true });
        send(
            &app,
            req(
                Method::PUT,
                "/api/v1/admin/samples/campaign",
                camp.clone(),
                &boss,
            ),
        )
        .await;
    }
    let id = camp["id"].as_str().unwrap().to_owned();
    sqlx::query("UPDATE sample_campaigns SET claims = 7 WHERE id = $1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    camp["claims"] = json!(0);
    let res = send(
        &app,
        req(Method::PUT, "/api/v1/admin/samples/campaign", camp, &boss),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let claims: i32 = sqlx::query_scalar("SELECT claims FROM sample_campaigns WHERE id = $1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(claims, 7);
}

/* ───────────────────────── Dashboard, reports, customers ───────────────────────── */

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn dashboard_reports_and_customers_show_real_orders(pool: PgPool) {
    let app = router(pool.clone());
    let support = staff(&app, &pool, "help@wagwell.example", "support").await;
    let me = sign_in(&app, &pool, "9876543210").await;
    let other = sign_in(&app, &pool, "9123456789").await;
    cod_order(&app, &me, "dash-key-0001", 2).await; // ₹1,898 + ₹29 COD = ₹1,927
    let gone = cod_order(&app, &other, "dash-key-0002", 1).await;
    // A cancelled order never counts as a sale.
    let res = send(
        &app,
        with_cookie(
            post_json(&format!("/api/v1/orders/{gone}/cancel"), json!({})),
            &other,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    sqlx::query("UPDATE variants SET stock = 2 WHERE id = 'so-1-2'")
        .execute(&pool)
        .await
        .unwrap();

    let d = json(send(&app, with_cookie(get("/api/v1/admin/dashboard"), &support)).await).await;
    assert_eq!(d["todayOrders"], 1, "{d}");
    assert_eq!(d["todayRevenuePaise"], 192_700);
    assert_eq!(d["revenuePaise"], 192_700);
    assert_eq!(d["toShip"], 1);
    assert_eq!(d["byDay"].as_array().unwrap().len(), 14);
    assert_eq!(d["byDay"][13]["revenuePaise"], 192_700);
    assert_eq!(d["topProducts"][0]["productId"], LRA.0);
    assert_eq!(d["topProducts"][0]["qty"], 2);
    assert!(
        d["lowStock"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["variantId"] == "so-1-2")
    );

    for kind in [
        "sales-day",
        "sales-product",
        "sales-category",
        "sales-payment",
        "gst",
        "inventory",
        "coupons",
        "customers",
    ] {
        let res = send(
            &app,
            with_cookie(get(&format!("/api/v1/admin/reports/{kind}")), &support),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK, "{kind}");
        let t = json(res).await;
        assert!(!t["head"].as_array().unwrap().is_empty(), "{kind}");
    }
    let day = json(
        send(
            &app,
            with_cookie(get("/api/v1/admin/reports/sales-day"), &support),
        )
        .await,
    )
    .await;
    assert_eq!(day["rows"][0][1], 1);
    assert_eq!(day["rows"][0][2], 2);
    assert_eq!(day["rows"][0][3], 1927.0);
    let gst = json(
        send(
            &app,
            with_cookie(get("/api/v1/admin/reports/gst"), &support),
        )
        .await,
    )
    .await;
    assert_eq!(gst["rows"][0][4], 1898.0, "goods only, after discounts");
    let res = send(
        &app,
        with_cookie(
            get("/api/v1/admin/reports/sales-day?from=2026-02-01&to=2026-01-01"),
            &support,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        with_cookie(get("/api/v1/admin/reports/everything"), &support),
    )
    .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let past = json(
        send(
            &app,
            with_cookie(
                get("/api/v1/admin/reports/sales-day?to=2020-01-01"),
                &support,
            ),
        )
        .await,
    )
    .await;
    assert!(past["rows"].as_array().unwrap().is_empty());

    let list = json(send(&app, with_cookie(get("/api/v1/admin/customers"), &support)).await).await;
    assert_eq!(list.as_array().unwrap().len(), 2);
    let found = json(
        send(
            &app,
            with_cookie(get("/api/v1/admin/customers?q=98765"), &support),
        )
        .await,
    )
    .await;
    assert_eq!(found.as_array().unwrap().len(), 1);
    assert_eq!(found[0]["orders"], 1);
    assert_eq!(found[0]["spentPaise"], 192_700);
    let one = json(
        send(
            &app,
            with_cookie(get("/api/v1/admin/customers/9123456789"), &support),
        )
        .await,
    )
    .await;
    assert_eq!(
        one["customer"]["spentPaise"], 0,
        "a cancelled order isn't spend"
    );
    assert_eq!(one["orders"][0]["status"], "cancelled");
    for bad in ["9999999999", "12345", "abc"] {
        let res = send(
            &app,
            with_cookie(get(&format!("/api/v1/admin/customers/{bad}")), &support),
        )
        .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{bad}");
    }
    // Customers can't see any of this.
    for path in [
        "/api/v1/admin/dashboard",
        "/api/v1/admin/customers",
        "/api/v1/admin/reports/sales-day",
    ] {
        let res = send(&app, with_cookie(get(path), &me)).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}

/* ───────────────────────── Sign-in codes ───────────────────────── */

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn without_an_sms_provider_no_code_is_pretended(pool: PgPool) {
    // Like production with no MSG91 keys: codes are not logged and not sent.
    let cfg = Config::from_lookup(|k| match k {
        "APP_ENV" => Some("development".into()),
        "DATABASE_URL" => Some("postgres://unused".into()),
        "OTP_PEPPER" => Some(PEPPER.into()),
        _ => None,
    })
    .unwrap();
    let app = app(AppState {
        db: pool.clone(),
        cfg: Arc::new(cfg),
        storage: None,
        razorpay: None,
        sms: None,
    });
    let res = send(
        &app,
        post_json(
            "/api/v1/auth/otp/request",
            json!({ "mobile": "9876543210" }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json(res).await["error"]["code"], "sms_unavailable");
    let codes: i64 = sqlx::query_scalar("SELECT count(*) FROM otp_codes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        codes, 0,
        "the unsent code is removed, so asking again isn't blocked"
    );
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn stock_log_ids_are_full_uuids(pool: PgPool) {
    let app = router(pool.clone());
    let boss = staff(&app, &pool, "boss@wagwell.example", "super_admin").await;
    let body = json!({ "productId": LRA.0, "variantId": LRA.1, "qty": 1, "type": "addition", "reason": "x" });
    send(
        &app,
        req(Method::POST, "/api/v1/admin/inventory/adjust", body, &boss),
    )
    .await;
    let id: String =
        sqlx::query_scalar("SELECT id FROM inventory_txns ORDER BY created_at DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(id.len(), "txn-".len() + 36, "{id}");
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn a_stock_count_is_applied_to_the_live_number(pool: PgPool) {
    let app = router(pool.clone());
    let boss = staff(&app, &pool, "boss@wagwell.example", "super_admin").await;
    let me = sign_in(&app, &pool, "9876543210").await;
    // The screen loaded with 42; an order then takes 2 (live stock 40); the shelf count says 38.
    cod_order(&app, &me, "count-key-0001", 2).await;
    let count = json!({ "productId": "someone-elses-product", "variantId": LRA.1, "counted": 38, "type": "adjustment", "reason": "Monthly count" });
    let res = send(
        &app,
        req(
            Method::POST,
            "/api/v1/admin/inventory/adjust",
            count.clone(),
            &boss,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let v = json(res).await;
    assert_eq!(
        (v["before"].as_i64(), v["after"].as_i64()),
        (Some(40), Some(38))
    );
    let (product, qty): (String, i32) =
        sqlx::query_as("SELECT product_id, qty FROM inventory_txns WHERE reason = 'Monthly count'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (product.as_str(), qty),
        (LRA.0, -2),
        "the variant's real product and the real change"
    );
    // Counting the same number again changes and records nothing.
    send(
        &app,
        req(Method::POST, "/api/v1/admin/inventory/adjust", count, &boss),
    )
    .await;
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM inventory_txns WHERE reason = 'Monthly count'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 1);
    let bad = json!({ "variantId": LRA.1, "counted": -1, "type": "adjustment", "reason": "x" });
    assert_eq!(
        send(
            &app,
            req(Method::POST, "/api/v1/admin/inventory/adjust", bad, &boss)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn popularity_counts_units_sold_and_survives_product_saves(pool: PgPool) {
    let app = router(pool.clone());
    let boss = staff(&app, &pool, "boss@wagwell.example", "super_admin").await;
    let me = sign_in(&app, &pool, "9876543210").await;
    let popularity = || async {
        sqlx::query_scalar::<_, i32>("SELECT popularity FROM products WHERE id = $1")
            .bind(LRA.0)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let start = popularity().await;
    let kept = cod_order(&app, &me, "pop-key-0001", 3).await;
    let gone = cod_order(&app, &me, "pop-key-0002", 2).await;
    assert_eq!(popularity().await, start + 5);
    send(
        &app,
        with_cookie(
            post_json(&format!("/api/v1/orders/{gone}/cancel"), json!({})),
            &me,
        ),
    )
    .await;
    assert_eq!(
        popularity().await,
        start + 3,
        "a cancelled order gives its units back"
    );
    let _ = kept;
    // An admin saving the product with an old copy doesn't reset the counter.
    let mut product = json(
        send(
            &app,
            with_cookie(get(&format!("/api/v1/admin/products/{}", LRA.0)), &boss),
        )
        .await,
    )
    .await;
    product["popularity"] = json!(0);
    assert_eq!(
        send(
            &app,
            req(
                Method::PUT,
                &format!("/api/v1/admin/products/{}", LRA.0),
                product,
                &boss
            )
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(popularity().await, start + 3);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn the_name_typed_at_sign_in_is_kept(pool: PgPool) {
    let app = router(pool.clone());
    let boss = staff(&app, &pool, "boss@wagwell.example", "super_admin").await;
    for (name, expect) in [
        (json!("  Priya   Raman "), "Priya Raman"),
        (json!(""), "Priya Raman"),
        (json!(null), "Priya Raman"),
    ] {
        sqlx::query("INSERT INTO otp_codes (mobile, code_hash, expires_at) VALUES ('9876543210', $1, now() + interval '5 minutes')")
            .bind(otp::hash_code(PEPPER, "9876543210", "123456"))
            .execute(&pool)
            .await
            .unwrap();
        let res = send(
            &app,
            post_json(
                "/api/v1/auth/otp/verify",
                json!({ "mobile": "9876543210", "code": "123456", "name": name }),
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let c = json(
            send(
                &app,
                with_cookie(get("/api/v1/admin/customers/9876543210"), &boss),
            )
            .await,
        )
        .await;
        assert_eq!(
            c["customer"]["name"], expect,
            "a blank name never wipes the saved one"
        );
    }
}
