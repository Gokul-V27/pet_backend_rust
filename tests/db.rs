//! Sign-in flows against a real PostgreSQL. Each test creates and drops its own throw-away database,
//! so point it at a LOCAL server, never at production (Supabase). Run with:
//!   DATABASE_URL=postgres://USER:PASS@localhost:5432/postgres cargo test --test db -- --ignored

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;
use sqlx::PgPool;
use wagwell_api::services::{otp, password};

const MOBILE: &str = "9876543210";

async fn plant_code(pool: &PgPool, code: &str) {
    sqlx::query("INSERT INTO otp_codes (mobile, code_hash, expires_at) VALUES ($1, $2, now() + interval '5 minutes')")
        .bind(MOBILE)
        .bind(otp::hash_code(PEPPER, MOBILE, code))
        .execute(pool)
        .await
        .unwrap();
}

async fn make_admin(pool: &PgPool, email: &str, pw: &str, role: &str) {
    sqlx::query("INSERT INTO admin_users (email, name, role, password_hash) VALUES ($1, 'Test Admin', $2, $3)")
        .bind(email)
        .bind(role)
        .bind(password::hash(pw).unwrap())
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn customer_signs_in_with_a_code_and_out_again(pool: PgPool) {
    let app = router(pool.clone());
    plant_code(&pool, "123456").await;

    let res = send(
        &app,
        post_json(
            "/api/v1/auth/otp/verify",
            json!({ "mobile": MOBILE, "code": "123456" }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = cookie_pair(&res);
    assert!(cookie.starts_with("wagwell_session="));
    assert_eq!(json(res).await["mobile"], MOBILE);

    let me = send(&app, with_cookie(get("/api/v1/auth/me"), &cookie)).await;
    assert_eq!(me.status(), StatusCode::OK);

    // The same code can't be used twice.
    let again = send(
        &app,
        post_json(
            "/api/v1/auth/otp/verify",
            json!({ "mobile": MOBILE, "code": "123456" }),
        ),
    )
    .await;
    assert_eq!(again.status(), StatusCode::UNAUTHORIZED);

    let out = send(
        &app,
        with_cookie(post_json("/api/v1/auth/logout", json!({})), &cookie),
    )
    .await;
    assert_eq!(out.status(), StatusCode::OK);
    let me = send(&app, with_cookie(get("/api/v1/auth/me"), &cookie)).await;
    assert_eq!(me.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn five_wrong_guesses_burn_the_code_even_if_the_sixth_is_right(pool: PgPool) {
    let app = router(pool.clone());
    plant_code(&pool, "123456").await;
    for _ in 0..5 {
        let res = send(
            &app,
            post_json(
                "/api/v1/auth/otp/verify",
                json!({ "mobile": MOBILE, "code": "000000" }),
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
    let res = send(
        &app,
        post_json(
            "/api/v1/auth/otp/verify",
            json!({ "mobile": MOBILE, "code": "123456" }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn asking_for_codes_is_rate_limited(pool: PgPool) {
    let app = router(pool);
    let first = send(
        &app,
        post_json("/api/v1/auth/otp/request", json!({ "mobile": MOBILE })),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let second = send(
        &app,
        post_json("/api/v1/auth/otp/request", json!({ "mobile": MOBILE })),
    )
    .await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(json(second).await["error"]["code"], "rate_limited");
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn staff_sign_in_lock_out_and_audit(pool: PgPool) {
    make_admin(
        &pool,
        "boss@wagwell.example",
        "a-long-enough-password",
        "super_admin",
    )
    .await;
    let app = router(pool.clone());
    let login = |pw: &str| {
        post_json(
            "/api/v1/admin/auth/login",
            json!({ "email": "Boss@Wagwell.example", "password": pw }),
        )
    };

    // Right password: signed in, and /me shows the role.
    let ok = send(&app, login("a-long-enough-password")).await;
    assert_eq!(ok.status(), StatusCode::OK);
    let cookie = cookie_pair(&ok);
    let me = send(&app, with_cookie(get("/api/v1/admin/auth/me"), &cookie)).await;
    assert_eq!(json(me).await["role"], "super_admin");

    // The shop's cookie name does not open the admin, and unknown emails look the same as wrong passwords.
    let unknown = send(
        &app,
        post_json(
            "/api/v1/admin/auth/login",
            json!({ "email": "nobody@x.example", "password": "whatever-whatever" }),
        ),
    )
    .await;
    assert_eq!(json(unknown).await["error"]["code"], "invalid_credentials");

    // Five wrong passwords lock the account, even for the right password afterwards.
    for _ in 0..5 {
        let bad = send(&app, login("not-the-password!")).await;
        assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);
    }
    let locked = send(&app, login("a-long-enough-password")).await;
    assert_eq!(locked.status(), StatusCode::TOO_MANY_REQUESTS);

    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_log ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(actions.contains(&"admin.login".to_owned()));
    assert!(actions.contains(&"admin.login_failed".to_owned()));
    assert!(actions.contains(&"admin.login_blocked".to_owned()));
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn staff_sessions_end_after_idle_time_and_on_logout(pool: PgPool) {
    make_admin(
        &pool,
        "boss@wagwell.example",
        "a-long-enough-password",
        "admin",
    )
    .await;
    let app = router(pool.clone());
    let ok = send(
        &app,
        post_json(
            "/api/v1/admin/auth/login",
            json!({ "email": "boss@wagwell.example", "password": "a-long-enough-password" }),
        ),
    )
    .await;
    let cookie = cookie_pair(&ok);

    sqlx::query("UPDATE admin_sessions SET last_seen_at = now() - interval '31 minutes'")
        .execute(&pool)
        .await
        .unwrap();
    let idle = send(&app, with_cookie(get("/api/v1/admin/auth/me"), &cookie)).await;
    assert_eq!(idle.status(), StatusCode::UNAUTHORIZED);

    let ok = send(
        &app,
        post_json(
            "/api/v1/admin/auth/login",
            json!({ "email": "boss@wagwell.example", "password": "a-long-enough-password" }),
        ),
    )
    .await;
    let cookie = cookie_pair(&ok);
    let out = send(
        &app,
        with_cookie(post_json("/api/v1/admin/auth/logout", json!({})), &cookie),
    )
    .await;
    assert_eq!(out.status(), StatusCode::OK);
    let me = send(&app, with_cookie(get("/api/v1/admin/auth/me"), &cookie)).await;
    assert_eq!(me.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "needs a PostgreSQL server: see the top of this file"]
async fn the_audit_log_cannot_be_edited_or_deleted(pool: PgPool) {
    sqlx::query("INSERT INTO audit_log (actor_type, action) VALUES ('system', 'test')")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE audit_log SET action = 'changed'")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM audit_log")
            .execute(&pool)
            .await
            .is_err()
    );
}
