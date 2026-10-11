//! Staff sign-in: email + password (Argon2id), then — when it's on — a code from an authenticator
//! app (two-step sign-in). Five wrong passwords or codes lock the account for 15 minutes. Wrong
//! email and wrong password give the same answer and take the same time. Every sign-in, failure,
//! two-step change and sign-out goes in the audit log.
//!
//! The admin website gets an HttpOnly cookie. The staff phone app (which sends
//! `X-Wagwell-Client: mobile`) gets the same kind of token once, as `token` in the body, to keep in
//! the phone's secure storage and send back as `Authorization: Bearer …`; its session lasts a shift.
//!
//! With two-step sign-in on, a right password only gets a short-lived `challenge` (5 minutes, 5
//! tries); `POST /auth/mfa/verify` with the challenge and a code starts the session. Super Admin and
//! Admin must turn it on when the server requires it (`STAFF_MFA_REQUIRED`): until then their
//! session reaches only the setup screens (see `auth.rs`).
//!
//! Staff can see their signed-in devices and sign any of them out (a lost phone, a shared PC).

use std::str::FromStr;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, header},
    routing::{get, post},
};
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::{
    ADMIN_COOKIE, ADMIN_IDLE_MINUTES, ADMIN_MOBILE_SESSION_HOURS, ADMIN_SESSION_HOURS, AdminAuth,
    clear_cookie, session_cookie,
};
use crate::error::{AppError, Result};
use crate::models::AdminRole;
use crate::repo::audit;
use crate::routes::security::is_mobile;
use crate::services::{password, secretbox, token, totp};
use crate::state::AppState;

const MAX_FAILED: i32 = 5;
const LOCK_MINUTES: i32 = 15;
/// How long the step between "password right" and "code right" lasts, and its tries.
const CHALLENGE_MINUTES: i32 = 5;
const CHALLENGE_TRIES: i32 = 5;
const ISSUER: &str = "Wagwell Admin";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/login", post(login))
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
        .route("/auth/mfa", get(mfa_status))
        .route("/auth/mfa/verify", post(mfa_verify))
        .route("/auth/mfa/setup", post(mfa_setup))
        .route("/auth/mfa/enable", post(mfa_enable))
        .route("/auth/mfa/disable", post(mfa_disable))
        .route("/auth/mfa/recovery-codes", post(new_recovery_codes))
        .route("/auth/sessions", get(list_sessions))
        .route("/auth/sessions/revoke-others", post(revoke_other_sessions))
        .route("/auth/sessions/{id}/revoke", post(revoke_session))
}

#[derive(Deserialize)]
struct Login {
    email: String,
    password: String,
}

type AdminRow = (Uuid, String, String, String, String, i32, bool, bool, bool);

/// Counts a wrong password or code; the fifth in a row locks the account for a while.
async fn count_failure(s: &AppState, id: Uuid) -> Result<()> {
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
    Ok(())
}

/// The device as its browser or app names itself, for the signed-in devices list.
fn user_agent(headers: &HeaderMap) -> Option<String> {
    let ua = headers.get(header::USER_AGENT)?.to_str().ok()?;
    let clean: String = ua.chars().filter(|c| !c.is_control()).take(200).collect();
    (!clean.trim().is_empty()).then_some(clean)
}

fn mfa_setup_required(s: &AppState, role: AdminRole, mfa_enabled: bool) -> bool {
    s.cfg.staff_mfa_required && role.must_use_mfa() && !mfa_enabled
}

/// Signs the staff member in: a cookie for the website, a token in the body for the phone app.
#[allow(clippy::too_many_arguments)]
async fn start_session(
    s: &AppState,
    jar: CookieJar,
    headers: &HeaderMap,
    mobile_app: bool,
    (id, email, name, role): (Uuid, String, String, AdminRole),
    mfa_enabled: bool,
    via: &str,
) -> Result<(CookieJar, Json<Value>)> {
    let (session_token, token_hash) = token::new_token()?;
    let (client, hours) = if mobile_app {
        ("mobile", ADMIN_MOBILE_SESSION_HOURS)
    } else {
        ("web", ADMIN_SESSION_HOURS)
    };
    let mut tx = s.db.begin().await?;
    sqlx::query("UPDATE admin_users SET failed_attempts = 0, locked_until = NULL, last_login_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO admin_sessions (admin_id, token_hash, expires_at, client, user_agent)
         VALUES ($1, $2, now() + ($3::int * interval '1 hour'), $4, $5)",
    )
    .bind(id)
    .bind(token_hash)
    .bind(hours as i32)
    .bind(client)
    .bind(user_agent(headers))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    audit::record(
        &s.db,
        "admin",
        Some(id),
        "admin.login",
        None,
        json!({ "client": client, "via": via }),
    )
    .await;
    let me = json!({
        "id": id, "email": email, "name": name, "role": role,
        "mfaEnabled": mfa_enabled, "mfaSetupRequired": mfa_setup_required(s, role, mfa_enabled),
    });
    if mobile_app {
        // The phone keeps the token; no cookie is set.
        return Ok((jar, Json(json!({ "token": session_token, "admin": me }))));
    }
    let jar = jar.add(session_cookie(
        ADMIN_COOKIE,
        session_token,
        s.cfg.cookie_secure(),
        time::Duration::hours(ADMIN_SESSION_HOURS),
    ));
    Ok((jar, Json(me)))
}

async fn login(
    State(s): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(req): Json<Login>,
) -> Result<(CookieJar, Json<Value>)> {
    let mobile_app = is_mobile(&headers);
    let email = req.email.trim().to_lowercase();
    if email.is_empty() || email.len() > 254 || req.password.is_empty() || req.password.len() > 256
    {
        return Err(AppError::InvalidCredentials);
    }

    let admin: Option<AdminRow> = sqlx::query_as(
        "SELECT id, email, name, role, password_hash, failed_attempts, active,
                COALESCE(locked_until > now(), false), mfa_enabled_at IS NOT NULL
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

    let Some((id, db_email, name, role, _hash, _failed, active, locked, mfa_on)) = admin else {
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
        count_failure(&s, id).await?;
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

    if mfa_on {
        // Password right: now the code. Nothing that signs in is handed out yet.
        let (challenge, challenge_hash) = token::new_token()?;
        sqlx::query(
            "INSERT INTO admin_mfa_challenges (admin_id, token_hash, client, expires_at)
             VALUES ($1, $2, $3, now() + ($4::int * interval '1 minute'))",
        )
        .bind(id)
        .bind(challenge_hash)
        .bind(if mobile_app { "mobile" } else { "web" })
        .bind(CHALLENGE_MINUTES)
        .execute(&s.db)
        .await?;
        return Ok((
            jar,
            Json(json!({
                "mfaRequired": true,
                "challenge": challenge,
                "expiresInSeconds": CHALLENGE_MINUTES * 60,
            })),
        ));
    }
    start_session(
        &s,
        jar,
        &headers,
        mobile_app,
        (id, db_email, name, role),
        false,
        "password",
    )
    .await
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

/* ───────────────────────── Two-step sign-in ───────────────────────── */

fn mfa_key(s: &AppState) -> Result<&[u8; 32]> {
    s.cfg
        .mfa_key
        .as_ref()
        .map(|k| &k.0)
        .ok_or(AppError::MfaUnavailable)
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Ten fresh recovery codes: the old ones stop working. Returns them in plain text, once.
async fn replace_recovery_codes(
    conn: &mut sqlx::PgConnection,
    admin_id: Uuid,
) -> Result<Vec<String>> {
    sqlx::query("DELETE FROM admin_recovery_codes WHERE admin_id = $1")
        .bind(admin_id)
        .execute(&mut *conn)
        .await?;
    let mut codes = Vec::with_capacity(totp::RECOVERY_CODES);
    for _ in 0..totp::RECOVERY_CODES {
        let code = totp::readable_code(10)?;
        sqlx::query("INSERT INTO admin_recovery_codes (admin_id, code_hash) VALUES ($1, $2)")
            .bind(admin_id)
            .bind(totp::hash_code("recovery", &code))
            .execute(&mut *conn)
            .await?;
        codes.push(code);
    }
    Ok(codes)
}

/// Checks a code against the staff member's authenticator secret, then their unused recovery
/// codes. Records what was used, so neither works twice. `Ok(None)` = wrong code.
async fn check_code(
    s: &AppState,
    conn: &mut sqlx::PgConnection,
    admin_id: Uuid,
    code: &str,
) -> Result<Option<&'static str>> {
    let key = mfa_key(s)?;
    let row: Option<(Option<String>, i64)> = sqlx::query_as(
        "SELECT mfa_secret_enc, mfa_last_step FROM admin_users WHERE id = $1 FOR UPDATE",
    )
    .bind(admin_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((Some(enc), last_step)) = row else {
        return Ok(None);
    };
    let secret = secretbox::open(key, &admin_id.to_string(), &enc)?;
    if let Some(step) = totp::verify(&secret, code, now_unix(), last_step) {
        sqlx::query("UPDATE admin_users SET mfa_last_step = $2 WHERE id = $1")
            .bind(admin_id)
            .bind(step)
            .execute(&mut *conn)
            .await?;
        return Ok(Some("authenticator"));
    }
    let used: Option<Uuid> = sqlx::query_scalar(
        "UPDATE admin_recovery_codes SET used_at = now()
          WHERE id = (SELECT id FROM admin_recovery_codes
                       WHERE admin_id = $1 AND code_hash = $2 AND used_at IS NULL LIMIT 1)
          RETURNING id",
    )
    .bind(admin_id)
    .bind(totp::hash_code("recovery", code))
    .fetch_optional(&mut *conn)
    .await?;
    Ok(used.map(|_| "recovery_code"))
}

#[derive(Deserialize)]
struct MfaVerify {
    challenge: String,
    code: String,
}

/// The second step of sign-in: the challenge from `login` plus a code.
async fn mfa_verify(
    State(s): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(req): Json<MfaVerify>,
) -> Result<(CookieJar, Json<Value>)> {
    let challenge = req.challenge.trim();
    if challenge.len() != 64
        || !challenge.bytes().all(|b| b.is_ascii_hexdigit())
        || req.code.len() > 40
    {
        return Err(AppError::MfaInvalid);
    }
    let mut tx = s.db.begin().await?;
    let row: Option<(Uuid, Uuid, String, i32)> = sqlx::query_as(
        "SELECT id, admin_id, client, attempts FROM admin_mfa_challenges
          WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now() FOR UPDATE",
    )
    .bind(token::hash_token(challenge))
    .fetch_optional(&mut *tx)
    .await?;
    let Some((challenge_id, admin_id, client, attempts)) = row else {
        return Err(AppError::MfaInvalid);
    };
    if attempts >= CHALLENGE_TRIES {
        return Err(AppError::MfaInvalid);
    }
    sqlx::query("UPDATE admin_mfa_challenges SET attempts = attempts + 1 WHERE id = $1")
        .bind(challenge_id)
        .execute(&mut *tx)
        .await?;

    let admin: (String, String, String, bool, bool) = sqlx::query_as(
        "SELECT email, name, role, active, COALESCE(locked_until > now(), false) FROM admin_users WHERE id = $1",
    )
    .bind(admin_id)
    .fetch_one(&mut *tx)
    .await?;
    let (email, name, role, active, locked) = admin;
    if !active || locked {
        tx.commit().await?;
        return Err(AppError::InvalidCredentials);
    }
    let Some(via) = check_code(&s, &mut tx, admin_id, &req.code).await? else {
        tx.commit().await?; // keep the spent try
        count_failure(&s, admin_id).await?;
        audit::record(
            &s.db,
            "admin",
            Some(admin_id),
            "admin.mfa_failed",
            None,
            json!({}),
        )
        .await;
        return Err(AppError::MfaInvalid);
    };
    sqlx::query("UPDATE admin_mfa_challenges SET used_at = now() WHERE id = $1")
        .bind(challenge_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let role = AdminRole::from_str(&role)
        .map_err(|_| AppError::Other(anyhow::anyhow!("unknown role in database")))?;
    // The device that typed the password decides cookie or token, not this request's header.
    start_session(
        &s,
        jar,
        &headers,
        client == "mobile",
        (admin_id, email, name, role),
        true,
        via,
    )
    .await
}

async fn mfa_status(State(s): State<AppState>, admin: AdminAuth) -> Result<Json<Value>> {
    let left: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_recovery_codes WHERE admin_id = $1 AND used_at IS NULL",
    )
    .bind(admin.id)
    .fetch_one(&s.db)
    .await?;
    Ok(Json(json!({
        "enabled": admin.mfa_enabled,
        "required": s.cfg.staff_mfa_required && admin.role.must_use_mfa(),
        "available": s.cfg.mfa_key.is_some(),
        "recoveryCodesLeft": left,
    })))
}

/// Starts turning two-step sign-in on: a new secret for the authenticator app. It only counts once
/// a code from it is confirmed with `/auth/mfa/enable`.
async fn mfa_setup(State(s): State<AppState>, admin: AdminAuth) -> Result<Json<Value>> {
    let key = mfa_key(&s)?;
    if admin.mfa_enabled {
        return Err(AppError::invalid(
            "Two-step sign-in is already on. To move it to a new phone, turn it off first.",
        ));
    }
    let secret = totp::new_secret()?;
    sqlx::query("UPDATE admin_users SET mfa_pending_enc = $2 WHERE id = $1")
        .bind(admin.id)
        .bind(secretbox::seal(key, &admin.id.to_string(), &secret)?)
        .execute(&s.db)
        .await?;
    Ok(Json(json!({
        "secret": totp::base32_encode(&secret),
        "otpauthUrl": totp::otpauth_url(ISSUER, &admin.email, &secret),
    })))
}

#[derive(Deserialize)]
struct Code {
    code: String,
}

/// Confirms the authenticator app with a code from it. Turns two-step sign-in on, signs out every
/// other device (anyone already in with just the password), and returns ten recovery codes, once.
async fn mfa_enable(
    State(s): State<AppState>,
    admin: AdminAuth,
    Json(req): Json<Code>,
) -> Result<Json<Value>> {
    let key = mfa_key(&s)?;
    let mut tx = s.db.begin().await?;
    let pending: Option<Option<String>> =
        sqlx::query_scalar("SELECT mfa_pending_enc FROM admin_users WHERE id = $1 FOR UPDATE")
            .bind(admin.id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(Some(enc)) = pending else {
        return Err(AppError::invalid(
            "Start the setup again: no authenticator app is waiting to be confirmed.",
        ));
    };
    let secret = secretbox::open(key, &admin.id.to_string(), &enc)?;
    let Some(step) = totp::verify(&secret, &req.code, now_unix(), 0) else {
        return Err(AppError::MfaInvalid);
    };
    sqlx::query(
        "UPDATE admin_users SET mfa_secret_enc = mfa_pending_enc, mfa_pending_enc = NULL,
                mfa_enabled_at = now(), mfa_last_step = $2 WHERE id = $1",
    )
    .bind(admin.id)
    .bind(step)
    .execute(&mut *tx)
    .await?;
    let codes = replace_recovery_codes(&mut tx, admin.id).await?;
    let others = sqlx::query(
        "UPDATE admin_sessions SET revoked_at = now() WHERE admin_id = $1 AND id <> $2 AND revoked_at IS NULL",
    )
    .bind(admin.id)
    .bind(admin.session_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    audit::record(
        &s.db,
        "admin",
        Some(admin.id),
        "admin.mfa_enabled",
        None,
        json!({ "otherSessionsEnded": others }),
    )
    .await;
    Ok(Json(
        json!({ "enabled": true, "recoveryCodes": codes, "otherSessionsEnded": others }),
    ))
}

async fn mfa_disable(
    State(s): State<AppState>,
    admin: AdminAuth,
    Json(req): Json<Code>,
) -> Result<Json<Value>> {
    if s.cfg.staff_mfa_required && admin.role.must_use_mfa() {
        return Err(AppError::invalid(
            "Your role must keep two-step sign-in on.",
        ));
    }
    if !admin.mfa_enabled {
        return Ok(Json(json!({ "enabled": false })));
    }
    let mut tx = s.db.begin().await?;
    if check_code(&s, &mut tx, admin.id, &req.code)
        .await?
        .is_none()
    {
        tx.commit().await?;
        return Err(AppError::MfaInvalid);
    }
    sqlx::query(
        "UPDATE admin_users SET mfa_secret_enc = NULL, mfa_pending_enc = NULL, mfa_enabled_at = NULL WHERE id = $1",
    )
    .bind(admin.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM admin_recovery_codes WHERE admin_id = $1")
        .bind(admin.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    audit::record(
        &s.db,
        "admin",
        Some(admin.id),
        "admin.mfa_disabled",
        None,
        json!({}),
    )
    .await;
    Ok(Json(json!({ "enabled": false })))
}

/// New recovery codes (the old ones stop working), after a code proves it's really them.
async fn new_recovery_codes(
    State(s): State<AppState>,
    admin: AdminAuth,
    Json(req): Json<Code>,
) -> Result<Json<Value>> {
    if !admin.mfa_enabled {
        return Err(AppError::invalid("Turn on two-step sign-in first."));
    }
    let mut tx = s.db.begin().await?;
    if check_code(&s, &mut tx, admin.id, &req.code)
        .await?
        .is_none()
    {
        tx.commit().await?;
        return Err(AppError::MfaInvalid);
    }
    let codes = replace_recovery_codes(&mut tx, admin.id).await?;
    tx.commit().await?;
    audit::record(
        &s.db,
        "admin",
        Some(admin.id),
        "admin.mfa_recovery_codes",
        None,
        json!({}),
    )
    .await;
    Ok(Json(json!({ "recoveryCodes": codes })))
}

/* ───────────────────────── Signed-in devices ───────────────────────── */

type SessionRow = (
    Uuid,
    String,
    Option<String>,
    chrono::DateTime<chrono::Utc>,
    chrono::DateTime<chrono::Utc>,
    chrono::DateTime<chrono::Utc>,
);

async fn list_sessions(State(s): State<AppState>, admin: AdminAuth) -> Result<Json<Vec<Value>>> {
    let rows: Vec<SessionRow> = sqlx::query_as(
        "SELECT id, client, user_agent, created_at, last_seen_at, expires_at FROM admin_sessions
          WHERE admin_id = $1 AND revoked_at IS NULL AND expires_at > now()
            AND (client = 'mobile' OR last_seen_at > now() - ($2::int * interval '1 minute'))
          ORDER BY last_seen_at DESC",
    )
    .bind(admin.id)
    .bind(ADMIN_IDLE_MINUTES as i32)
    .fetch_all(&s.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, client, ua, created, seen, expires)| {
                json!({
                    "id": id, "client": client, "userAgent": ua, "createdAt": created,
                    "lastSeenAt": seen, "expiresAt": expires, "current": id == admin.session_id,
                })
            })
            .collect(),
    ))
}

async fn revoke_session(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let n = sqlx::query(
        "UPDATE admin_sessions SET revoked_at = now() WHERE id = $1 AND admin_id = $2 AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(admin.id)
    .execute(&s.db)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound);
    }
    audit::record(
        &s.db,
        "admin",
        Some(admin.id),
        "admin.session_revoked",
        Some(&id.to_string()),
        json!({}),
    )
    .await;
    Ok(Json(json!({ "revoked": id })))
}

async fn revoke_other_sessions(State(s): State<AppState>, admin: AdminAuth) -> Result<Json<Value>> {
    let n = sqlx::query(
        "UPDATE admin_sessions SET revoked_at = now() WHERE admin_id = $1 AND id <> $2 AND revoked_at IS NULL",
    )
    .bind(admin.id)
    .bind(admin.session_id)
    .execute(&s.db)
    .await?
    .rows_affected();
    audit::record(
        &s.db,
        "admin",
        Some(admin.id),
        "admin.sessions_revoked",
        None,
        json!({ "count": n }),
    )
    .await;
    Ok(Json(json!({ "revoked": n })))
}
