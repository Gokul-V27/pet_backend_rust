//! HTTP-level checks that don't need a database.

mod common;

use axum::http::{Request, StatusCode, header};
use common::*;
use serde_json::json;

#[tokio::test]
async fn health_is_ok_and_responses_carry_safety_headers() {
    let app = router(lazy_pool());
    let res = send(&app, get("/health")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let h = res.headers();
    assert_eq!(h[header::CACHE_CONTROL], "no-store");
    assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(h[header::X_FRAME_OPTIONS], "DENY");
    assert_eq!(json(res).await["status"], "ok");
}

#[tokio::test]
async fn changing_requests_without_the_client_header_are_refused() {
    let app = router(lazy_pool());
    let req = Request::post("/api/v1/auth/otp/request")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            json!({ "mobile": "9876543210" }).to_string(),
        ))
        .unwrap();
    let res = send(&app, req).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(json(res).await["error"]["code"], "forbidden");
}

#[tokio::test]
async fn signed_out_calls_get_a_clean_401() {
    let app = router(lazy_pool());
    for path in ["/api/v1/auth/me", "/api/v1/admin/auth/me"] {
        let res = send(&app, get(path)).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{path}");
        let body = json(res).await;
        assert_eq!(body["error"]["code"], "unauthorized");
        assert!(body["error"]["message"].is_string());
    }
}

#[tokio::test]
async fn bad_mobile_is_rejected_before_touching_the_database() {
    let app = router(lazy_pool());
    let res = send(
        &app,
        post_json("/api/v1/auth/otp/request", json!({ "mobile": "12345" })),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json(res).await["error"]["code"], "invalid_input");
}

#[tokio::test]
async fn malformed_codes_are_rejected_without_a_database() {
    let app = router(lazy_pool());
    let res = send(
        &app,
        post_json(
            "/api/v1/auth/otp/verify",
            json!({ "mobile": "9876543210", "code": "12ab56" }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(json(res).await["error"]["code"], "otp_invalid");
}

#[tokio::test]
async fn cors_allows_our_two_websites_with_cookies_and_nobody_else() {
    let app = router(lazy_pool());
    let preflight = |origin: &str| {
        Request::builder()
            .method("OPTIONS")
            .uri("/api/v1/auth/me")
            .header(header::ORIGIN, origin)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(
                header::ACCESS_CONTROL_REQUEST_HEADERS,
                "content-type,idempotency-key,x-wagwell-client",
            )
            .body(axum::body::Body::empty())
            .unwrap()
    };
    for ok in ["http://localhost:3000", "http://localhost:3001"] {
        let res = send(&app, preflight(ok)).await;
        assert_eq!(res.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], ok);
        assert_eq!(
            res.headers()[header::ACCESS_CONTROL_ALLOW_CREDENTIALS],
            "true"
        );
        // Placing an order sends an Idempotency-Key: the browser's preflight must be allowed it.
        let allowed = res.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS]
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(allowed.contains("idempotency-key"), "{allowed}");
    }
    let res = send(&app, preflight("https://evil.example")).await;
    assert!(
        res.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none()
    );
}

#[tokio::test]
async fn image_upload_needs_a_staff_session() {
    let app = router(lazy_pool());
    let req = Request::post("/api/v1/admin/images")
        .header(header::CONTENT_TYPE, "multipart/form-data; boundary=x")
        .header("x-wagwell-client", "web")
        .body(axum::body::Body::from("--x--\r\n"))
        .unwrap();
    let res = send(&app, req).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn only_catalogue_roles_may_manage_pictures() {
    use wagwell_api::models::AdminRole::*;
    assert!(
        SuperAdmin.can_manage_catalogue()
            && Admin.can_manage_catalogue()
            && InventoryManager.can_manage_catalogue()
    );
    assert!(!OrderManager.can_manage_catalogue() && !Support.can_manage_catalogue());
}

#[tokio::test]
async fn orders_and_finance_need_the_right_session() {
    let app = router(lazy_pool());
    for path in ["/api/v1/orders", "/api/v1/orders/WAG-100001"] {
        let res = send(&app, get(path)).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
    let res = send(&app, post_json("/api/v1/orders", json!({}))).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    for path in [
        "/api/v1/admin/orders",
        "/api/v1/admin/finance/trial-balance",
        "/api/v1/admin/finance/journal",
        "/api/v1/admin/finance/pnl",
        "/api/v1/admin/finance/gst",
    ] {
        let res = send(&app, get(path)).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[tokio::test]
async fn order_and_payment_calls_need_the_client_header() {
    let app = router(lazy_pool());
    for path in [
        "/api/v1/orders",
        "/api/v1/payments/razorpay/verify",
        "/api/v1/admin/orders/WAG-1/refund",
    ] {
        let req = Request::post(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from("{}"))
            .unwrap();
        assert_eq!(
            send(&app, req).await.status(),
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
}

#[tokio::test]
async fn webhook_is_off_without_razorpay_and_refuses_bad_signatures() {
    let body = r#"{"event":"payment.captured","payload":{}}"#;
    let hook = |sig: &str| {
        Request::post("/api/v1/payments/razorpay/webhook")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-razorpay-signature", sig)
            .body(axum::body::Body::from(body))
            .unwrap()
    };
    // No keys: the route answers like it doesn't exist.
    let off = router(lazy_pool());
    assert_eq!(send(&off, hook("00")).await.status(), StatusCode::NOT_FOUND);
    // With keys: no website header needed, but a wrong or missing signature is refused before any database work.
    let on = router_with_razorpay(lazy_pool());
    assert_eq!(
        send(&on, hook("deadbeef")).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(send(&on, hook("")).await.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn staff_screens_need_a_staff_session() {
    let app = router(lazy_pool());
    for path in [
        "/api/v1/admin/dashboard",
        "/api/v1/admin/reports/sales-day",
        "/api/v1/admin/customers",
        "/api/v1/admin/customers/9876543210",
        "/api/v1/admin/audit",
    ] {
        let res = send(&app, get(path)).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[test]
fn each_role_may_change_only_what_the_admin_table_says() {
    use wagwell_api::models::AdminRole::*;
    let all = [SuperAdmin, Admin, OrderManager, InventoryManager, Support];
    let who = |f: fn(wagwell_api::models::AdminRole) -> bool| {
        all.into_iter().filter(|r| f(*r)).collect::<Vec<_>>()
    };
    assert_eq!(who(|r| r.can_edit_catalogue()), [SuperAdmin, Admin]);
    assert_eq!(who(|r| r.can_run_marketing()), [SuperAdmin, Admin]);
    assert_eq!(
        who(|r| r.can_adjust_stock()),
        [SuperAdmin, Admin, InventoryManager]
    );
    assert_eq!(who(|r| r.can_edit_messages()), [SuperAdmin, Admin, Support]);
    assert_eq!(who(|r| r.can_view_audit()), [SuperAdmin]);
}
