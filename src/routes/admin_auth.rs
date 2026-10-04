//! Staff sign-in: email + password (Argon2id). Five wrong passwords lock the account for 15
//! minutes. Wrong email and wrong password give the same answer and take the same time. Every
//! sign-in, failure and sign-out goes in the audit log. (Two-step codes are the next addition.)

use std::str::FromStr;

use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::{ADMIN_COOKIE, ADMIN_SESSION_HOURS, AdminAuth, clear_cookie, session_cookie};
use crate::error::{AppError, Result};
use crate::models::AdminRole;
use crate::repo::audit;
use crate::services::{password, token};
use crate::state::AppState;

const MAX_FAILED: i32 = 5;
const LOCK_MINUTES: i32 = 15;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/login", post(login))
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
}

#[derive(Deserialize)]
struct Login {
    email: String,
    password: String,
}

type AdminRow = (Uuid, String, String, String, String, i32, bool, bool);

async fn login(
    State(s): State<AppState>,
    jar: CookieJar,
    Json(req): Json<Login>,
) -> Result<(CookieJar, Json<Value>)> {
    let email = req.email.trim().to_lowercase();
    if email.is_empty() || email.len() > 254 || req.password.is_empty() || req.password.len() > 256
    {
        return Err(AppError::InvalidCredentials);
    }

    let admin: Option<AdminRow> = sqlx::query_as(
        "SELECT id, email, name, role, password_hash, failed_attempts, active,
                COALESCE(locked_until > now(), false)
           FROM admin_users WHERE lower(email) = $1",
    )
    .bind(&email)
    .fetch_optional(&s.db)
    .await?;

    // Checking a password is slow on purpose, so do it off the async threads. When the email isn't
    // staff we still check against a dummy hash, so both cases take the same time.
    let stored = admin
        .as_ref()
        .map(|a| a.4.clone())
        .unwrap_or_else(|| password::dummy_hash().to_owned());
    let supplied = req.password;
    let password_ok = tokio::task::spawn_blocking(move || password::verify(&supplied, &stored))
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("password check failed: {e}")))?;

    let Some((id, db_email, name, role, _hash, _failed, active, locked)) = admin else {
        audit::record(
            &s.db,
            "system",
            None,
            "admin.login_failed",
            None,
            json!({ "reason": "unknown_email" }),
        )
        .await;
        return Err(AppError::InvalidCredentials);
    };
    if locked {
        audit::record(
            &s.db,
            "admin",
            Some(id),
            "admin.login_blocked",
            None,
            json!({ "reason": "locked" }),
        )
        .await;
        return Err(AppError::RateLimited(format!(
            "too many wrong attempts; try again in {LOCK_MINUTES} minutes"
        )));
    }
    if !password_ok || !active {
        sqlx::query(
            "UPDATE admin_users SET failed_attempts = failed_attempts + 1,
                    locked_until = CASE WHEN failed_attempts + 1 >= $2 THEN now() + ($3::int * interval '1 minute') ELSE locked_until END
              WHERE id = $1",
        )
        .bind(id)
        .bind(MAX_FAILED)
        .bind(LOCK_MINUTES)
        .execute(&s.db)
        .await?;
        audit::record(
            &s.db,
            "admin",
            Some(id),
            "admin.login_failed",
            None,
            json!({ "reason": if active { "bad_password" } else { "inactive" } }),
        )
        .await;
        return Err(AppError::InvalidCredentials);
    }

    let role = AdminRole::from_str(&role)
        .map_err(|_| AppError::Other(anyhow::anyhow!("unknown role in database")))?;
    let (cookie_token, token_hash) = token::new_token()?;
    let mut tx = s.db.begin().await?;
    sqlx::query("UPDATE admin_users SET failed_attempts = 0, locked_until = NULL, last_login_at = now() WHERE id = $1").bind(id).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO admin_sessions (admin_id, token_hash, expires_at) VALUES ($1, $2, now() + ($3::int * interval '1 hour'))")
        .bind(id)
        .bind(token_hash)
        .bind(ADMIN_SESSION_HOURS as i32)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    audit::record(&s.db, "admin", Some(id), "admin.login", None, json!({})).await;
    let jar = jar.add(session_cookie(
        ADMIN_COOKIE,
        cookie_token,
        s.cfg.cookie_secure(),
        time::Duration::hours(ADMIN_SESSION_HOURS),
    ));
    Ok((
        jar,
        Json(json!({ "id": id, "email": db_email, "name": name, "role": role })),
    ))
}

async fn me(admin: AdminAuth) -> Json<AdminAuth> {
    Json(admin)
}

async fn logout(
    State(s): State<AppState>,
    admin: AdminAuth,
    jar: CookieJar,
) -> Result<(CookieJar, Json<Value>)> {
    sqlx::query("UPDATE admin_sessions SET revoked_at = now() WHERE id = $1")
        .bind(admin.session_id)
        .execute(&s.db)
        .await?;
    audit::record(
        &s.db,
        "admin",
        Some(admin.id),
        "admin.logout",
        None,
        json!({}),
    )
    .await;
    Ok((
        jar.add(clear_cookie(ADMIN_COOKIE, s.cfg.cookie_secure())),
        Json(json!({ "signed_out": true })),
    ))
}
