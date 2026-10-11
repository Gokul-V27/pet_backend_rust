//! Phone apps and own delivery riders end to end, against a real PostgreSQL: the customer app signs
//! in with a bearer token, staff add and assign a rider, the rider app starts the delivery and
//! shares its position, the customer sees it on the tracking map, and the handover code completes it.
//! Each test gets its own throw-away database, so point it at a LOCAL server, never at Supabase:
//!   DATABASE_URL=postgres://USER:PASS@localhost:5432/postgres cargo test --test delivery_db -- --ignored

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::*;
use serde_json::{Value, json};
use sqlx::PgPool;
use wagwell_api::services::{otp, password};

const PASSWORD: &str = "a-long-enough-password";

fn mobile_post(path: &str, token: Option<&str>, body: Value) -> Request<Body> {
    let mut req = Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-wagwell-client", "mobile");
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    req.body(Body::from(body.to_string())).unwrap()
}

fn mobile_get(path: &str, token: &str) -> Request<Body> {
    Request::get(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

/// The customer app's sign-in: the token comes back in the body, and no cookie is set.
async fn app_sign_in(app: &Router, pool: &PgPool, mobile: &str) -> String {
    sqlx::query("INSERT INTO otp_codes (mobile, code_hash, expires_at) VALUES ($1, $2, now() + interval '5 minutes')")
        .bind(mobile)
        .bind(otp::hash_code(PEPPER, mobile, "123456"))
        .execute(pool)
        .await
        .unwrap();
    let res = send(
        app,
        mobile_post(
            "/api/v1/auth/otp/verify",
            None,
            json!({ "mobile": mobile, "code": "123456" }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        res.headers().get(header::SET_COOKIE).is_none(),
        "the app gets no cookie"
    );
    let body = json(res).await;
    assert_eq!(body["mobile"], mobile);
    body["token"].as_str().expect("token in body").to_owned()
}

async fn staff(app: &Router, pool: &PgPool, email: &str, role: &str) -> String {
    sqlx::query(
        "INSERT INTO admin_users (email, name, role, password_hash) VALUES ($1, 'Test', $2, $3)",
    )
    .bind(email)
    .bind(role)
    .bind(password::hash(PASSWORD).unwrap())
    .execute(pool)
    .await
    .unwrap();
    let res = send(
        app,
        post_json(
            "/api/v1/admin/auth/login",
            json!({ "email": email, "password": PASSWORD }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    cookie_pair(&res)
}

async fn rider_login(app: &Router, mobile: &str) -> String {
    let res = send(
        app,
        mobile_post(
            "/api/v1/rider/auth/login",
            None,
            json!({ "mobile": mobile, "password": PASSWORD }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    json(res).await["token"].as_str().unwrap().to_owned()
}

async fn place_order(app: &Router, token: &str) -> String {
    let mut req = mobile_post(
        "/api/v1/orders",
        Some(token),
        json!({
            "lines": [{ "productId": "lamb-rice-adult", "variantId": "lra-1-2", "qty": 1 }],
            "address": { "name": "Priya Raman", "phone": "9876543210", "line1": "12 Test St", "area": "Adyar",
                         "city": "Chennai", "pincode": "600020", "lat": 13.0012, "lng": 80.2565 },
            "slot": { "date": wagwell_api::services::orders::today_ist().succ_opt().unwrap(),
                      "label": "Tomorrow, 9 am – 1 pm", "kind": "standard" },
            "payment": "cod",
        }),
    );
    req.headers_mut()
        .insert("idempotency-key", "app-checkout-0001".parse().unwrap());
    let res = send(app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let order = json(res).await;
    assert_eq!(order["paymentStatus"], "cod-due");
    order["id"].as_str().unwrap().to_owned()
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn rider_delivers_a_cash_order_while_the_customer_watches(pool: PgPool) {
    let app = router(pool.clone());
    let customer = app_sign_in(&app, &pool, "9876543210").await;
    let me = send(&app, mobile_get("/api/v1/auth/me", &customer)).await;
    assert_eq!(me.status(), StatusCode::OK, "bearer token works");
    let number = place_order(&app, &customer).await;

    // Only owners add riders; order managers assign them.
    let owner = staff(&app, &pool, "owner@wagwell.in", "admin").await;
    let manager = staff(&app, &pool, "orders@wagwell.in", "order_manager").await;
    let new_rider = json!({ "mobile": "9000000001", "name": "Ravi Kumar", "password": PASSWORD });
    let refused = send(
        &app,
        with_cookie(
            post_json("/api/v1/admin/riders", new_rider.clone()),
            &manager,
        ),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let created = send(
        &app,
        with_cookie(post_json("/api/v1/admin/riders", new_rider), &owner),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    let rider_id = json(created).await["id"].as_str().unwrap().to_owned();
    sqlx::query("INSERT INTO riders (mobile, name, password_hash) VALUES ('9000000002', 'Other Rider', $1)")
        .bind(password::hash(PASSWORD).unwrap())
        .execute(&pool)
        .await
        .unwrap();

    // Packed and handed to Ravi.
    let pack = send(
        &app,
        with_cookie(
            post_json(
                &format!("/api/v1/admin/orders/{number}/status"),
                json!({ "status": "packed" }),
            ),
            &manager,
        ),
    )
    .await;
    assert_eq!(pack.status(), StatusCode::OK);
    let assign = send(
        &app,
        with_cookie(
            post_json(
                &format!("/api/v1/admin/orders/{number}/rider"),
                json!({ "riderId": rider_id }),
            ),
            &manager,
        ),
    )
    .await;
    assert_eq!(assign.status(), StatusCode::OK);
    let admin_view = json(
        send(
            &app,
            with_cookie(get(&format!("/api/v1/admin/orders/{number}")), &manager),
        )
        .await,
    )
    .await;
    assert_eq!(admin_view["rider"]["name"], "Ravi Kumar");

    // Before it leaves, the customer sees the drop pin but no rider or code.
    let track = |t: &str| mobile_get(&format!("/api/v1/orders/{number}/tracking"), t);
    let before = json(send(&app, track(&customer)).await).await;
    assert_eq!(before["drop"]["lat"], 13.0012);
    assert!(before["rider"].is_null() && before["deliveryCode"].is_null());

    // Ravi sees it; the other rider doesn't.
    let ravi = rider_login(&app, "9000000001").await;
    let other = rider_login(&app, "9000000002").await;
    let stops = json(send(&app, mobile_get("/api/v1/rider/orders", &ravi)).await).await;
    assert_eq!(stops[0]["number"], number.as_str());
    assert_eq!(stops[0]["collect"], stops[0]["total"]);
    assert_eq!(stops[0]["address"]["lng"], 80.2565);
    let others = json(send(&app, mobile_get("/api/v1/rider/orders", &other)).await).await;
    assert_eq!(others.as_array().unwrap().len(), 0);
    let peek = send(
        &app,
        mobile_post(
            &format!("/api/v1/rider/orders/{number}/start"),
            Some(&other),
            json!({}),
        ),
    )
    .await;
    assert_eq!(peek.status(), StatusCode::NOT_FOUND);

    // Out for delivery, sharing position: the customer sees Ravi's first name, his position and the code.
    let start = send(
        &app,
        mobile_post(
            &format!("/api/v1/rider/orders/{number}/start"),
            Some(&ravi),
            json!({}),
        ),
    )
    .await;
    assert_eq!(start.status(), StatusCode::OK);
    assert_eq!(json(start).await["status"], "out-for-delivery");
    let ping = send(
        &app,
        mobile_post(
            "/api/v1/rider/location",
            Some(&ravi),
            json!({ "lat": 13.01, "lng": 80.25, "heading": 90.0 }),
        ),
    )
    .await;
    assert_eq!(ping.status(), StatusCode::OK);
    let live = json(send(&app, track(&customer)).await).await;
    assert_eq!(live["status"], "out-for-delivery");
    assert_eq!(live["rider"]["name"], "Ravi");
    assert_eq!(live["rider"]["location"]["lat"], 13.01);
    let code = live["deliveryCode"].as_str().unwrap().to_owned();
    assert_eq!(code.len(), 4);

    // Someone else can't track this order.
    let stranger = app_sign_in(&app, &pool, "9123456789").await;
    assert_eq!(
        send(&app, track(&stranger)).await.status(),
        StatusCode::NOT_FOUND
    );

    // A wrong code is refused and counted; the right one delivers it and books the cash.
    let wrong = if code == "0000" { "1111" } else { "0000" };
    let deliver = |c: &str| {
        mobile_post(
            &format!("/api/v1/rider/orders/{number}/deliver"),
            Some(&ravi),
            json!({ "code": c }),
        )
    };
    let bad = send(&app, deliver(wrong)).await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    assert!(
        json(bad).await["error"]["message"]
            .as_str()
            .unwrap()
            .contains("4 tries left")
    );
    let done = send(&app, deliver(&code)).await;
    assert_eq!(done.status(), StatusCode::OK);
    assert_eq!(json(done).await["status"], "delivered");

    let order = json(
        send(
            &app,
            mobile_get(&format!("/api/v1/orders/{number}"), &customer),
        )
        .await,
    )
    .await;
    assert_eq!(order["status"], "delivered");
    assert_eq!(order["paymentStatus"], "paid");
    let after = json(send(&app, track(&customer)).await).await;
    assert!(
        after["rider"].is_null() && after["deliveryCode"].is_null(),
        "nothing shared after delivery"
    );
    let booked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM journal_entries e JOIN orders o ON o.id = e.source_id WHERE o.number = $1 AND e.purpose = 'delivered'",
    )
    .bind(&number)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(booked, 1, "the sale is in the books");

    // Switching Ravi off signs him out and hides his position.
    let off = send(
        &app,
        with_cookie(
            post_json(
                &format!("/api/v1/admin/riders/{rider_id}/active"),
                json!({ "active": false }),
            ),
            &owner,
        ),
    )
    .await;
    assert_eq!(off.status(), StatusCode::OK);
    assert_eq!(
        send(&app, mobile_get("/api/v1/rider/orders", &ravi))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn too_many_wrong_codes_lock_the_handover(pool: PgPool) {
    let app = router(pool.clone());
    let customer = app_sign_in(&app, &pool, "9876543210").await;
    let number = place_order(&app, &customer).await;
    sqlx::query("INSERT INTO riders (mobile, name, password_hash) VALUES ('9000000001', 'Ravi', $1)")
        .bind(password::hash(PASSWORD).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE orders SET status = 'packed', rider_id = (SELECT id FROM riders) WHERE number = $1")
        .bind(&number)
        .execute(&pool)
        .await
        .unwrap();
    let ravi = rider_login(&app, "9000000001").await;
    let start = send(
        &app,
        mobile_post(
            &format!("/api/v1/rider/orders/{number}/start"),
            Some(&ravi),
            json!({}),
        ),
    )
    .await;
    assert_eq!(start.status(), StatusCode::OK);
    let code: String = sqlx::query_scalar("SELECT delivery_code FROM orders WHERE number = $1")
        .bind(&number)
        .fetch_one(&pool)
        .await
        .unwrap();
    let wrong = if code == "0000" { "1111" } else { "0000" };
    for _ in 0..5 {
        let res = send(
            &app,
            mobile_post(
                &format!("/api/v1/rider/orders/{number}/deliver"),
                Some(&ravi),
                json!({ "code": wrong }),
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
    // Even the right code is refused now: the shop has to confirm it.
    let res = send(
        &app,
        mobile_post(
            &format!("/api/v1/rider/orders/{number}/deliver"),
            Some(&ravi),
            json!({ "code": code }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
}
