//! Shop sign-in: enter a mobile number, get a 6-digit code, enter it, you're signed in.
//! Wrong guesses are limited, requests per number are limited, and the answer to "send me a code"
//! is the same whether or not the number already has an account.

use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::{
    CUSTOMER_COOKIE, CUSTOMER_SESSION_DAYS, CustomerAuth, clear_cookie, session_cookie,
};
use crate::error::{AppError, Result};
use crate::models::Customer;
use crate::repo::audit;
use crate::services::{otp, token};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/otp/request", post(request_code))
        .route("/auth/otp/verify", post(verify_code))
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
}

#[derive(Deserialize)]
struct RequestCode {
    mobile: String,
}

async fn request_code(
    State(s): State<AppState>,
    Json(req): Json<RequestCode>,
) -> Result<Json<Value>> {
    let mobile = req.mobile.trim().to_owned();
    if !otp::valid_mobile(&mobile) {
        return Err(AppError::invalid("enter a 10-digit mobile number"));
    }

    let (last_hour, too_soon): (i64, bool) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE created_at > now() - interval '1 hour'),
                COALESCE(bool_or(created_at > now() - ($2::int * interval '1 second')), false)
           FROM otp_codes WHERE mobile = $1",
    )
    .bind(&mobile)
    .bind(otp::RESEND_AFTER_SECONDS as i32)
    .fetch_one(&s.db)
    .await?;
    if too_soon {
        return Err(AppError::RateLimited(format!(
            "wait {} seconds before asking for another code",
            otp::RESEND_AFTER_SECONDS
        )));
    }
    if last_hour >= otp::MAX_PER_HOUR {
        return Err(AppError::RateLimited(
            "too many codes for this number; try again in an hour".into(),
        ));
    }

    let code = otp::new_code()?;
    let code_id: uuid::Uuid = sqlx::query_scalar("INSERT INTO otp_codes (mobile, code_hash, expires_at) VALUES ($1, $2, now() + ($3::int * interval '1 minute')) RETURNING id")
        .bind(&mobile)
        .bind(otp::hash_code(&s.cfg.otp_pepper, &mobile, &code))
        .bind(otp::CODE_LIFETIME_MINUTES as i32)
        .fetch_one(&s.db)
        .await?;

    if s.cfg.otp_dev_echo {
        // Development only (refused in production by Config): no SMS provider needed to test sign-in.
        tracing::warn!(%mobile, %code, "DEV ONLY: one-time code");
    } else {
        // Fail closed: never say a code was sent when it wasn't. The unsent code is removed so the
        // customer can ask again straight away.
        let sent = match &s.sms {
            Some(sms) => sms.send_code(&mobile, &code).await,
            None => Err(anyhow::anyhow!("no SMS provider configured")),
        };
        if let Err(e) = sent {
            tracing::error!(error = ?e, "sign-in code not sent");
            sqlx::query("DELETE FROM otp_codes WHERE id = $1")
                .bind(code_id)
                .execute(&s.db)
                .await?;
            return Err(AppError::SmsUnavailable);
        }
    }

    Ok(Json(json!({
        "sent": true,
        "resend_after_seconds": otp::RESEND_AFTER_SECONDS,
        "expires_in_minutes": otp::CODE_LIFETIME_MINUTES,
    })))
}

#[derive(Deserialize)]
struct VerifyCode {
    mobile: String,
    code: String,
    /// The name the customer typed on the sign-in form, if any. Kept when it's blank.
    #[serde(default)]
    name: Option<String>,
}

/// A display name from the sign-in form: printable characters only, at most 80, never blank.
fn clean_name(raw: Option<&str>) -> Option<String> {
    let name: String = raw?.chars().filter(|c| !c.is_control()).take(80).collect();
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty()).then_some(name)
}

async fn verify_code(
    State(s): State<AppState>,
    jar: CookieJar,
    Json(req): Json<VerifyCode>,
) -> Result<(CookieJar, Json<Customer>)> {
    let mobile = req.mobile.trim().to_owned();
    let code = req.code.trim().to_owned();
    if !otp::valid_mobile(&mobile) || code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AppError::OtpInvalid);
    }

    let mut tx = s.db.begin().await?;
    // Lock the newest unused code so two requests can't both spend tries on it at once.
    let row: Option<(Uuid, String, i32)> = sqlx::query_as(
        "SELECT id, code_hash, attempts FROM otp_codes
          WHERE mobile = $1 AND consumed_at IS NULL AND expires_at > now()
          ORDER BY created_at DESC LIMIT 1 FOR UPDATE",
    )
    .bind(&mobile)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((otp_id, stored_hash, attempts)) = row else {
        return Err(AppError::OtpInvalid);
    };
    if attempts >= otp::MAX_ATTEMPTS {
        return Err(AppError::OtpInvalid);
    }
    sqlx::query("UPDATE otp_codes SET attempts = attempts + 1 WHERE id = $1")
        .bind(otp_id)
        .execute(&mut *tx)
        .await?;

    if !otp::matches(&stored_hash, &s.cfg.otp_pepper, &mobile, &code) {
        tx.commit().await?; // keep the failed try on record
        return Err(AppError::OtpInvalid);
    }
    sqlx::query("UPDATE otp_codes SET consumed_at = now() WHERE id = $1")
        .bind(otp_id)
        .execute(&mut *tx)
        .await?;

    let customer: Customer = sqlx::query_as(
        "INSERT INTO customers (mobile, name) VALUES ($1, $2)
         ON CONFLICT (mobile) DO UPDATE SET name = COALESCE($2, customers.name), updated_at = now()
         RETURNING id, mobile, name, email",
    )
    .bind(&mobile)
    .bind(clean_name(req.name.as_deref()))
    .fetch_one(&mut *tx)
    .await?;

    let (cookie_token, token_hash) = token::new_token()?;
    sqlx::query("INSERT INTO customer_sessions (customer_id, token_hash, expires_at) VALUES ($1, $2, now() + ($3::int * interval '1 day'))")
        .bind(customer.id)
        .bind(token_hash)
        .bind(CUSTOMER_SESSION_DAYS as i32)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    audit::record(
        &s.db,
        "customer",
        Some(customer.id),
        "customer.sign_in",
        None,
        json!({}),
    )
    .await;
    let jar = jar.add(session_cookie(
        CUSTOMER_COOKIE,
        cookie_token,
        s.cfg.cookie_secure(),
        time::Duration::days(CUSTOMER_SESSION_DAYS),
    ));
    Ok((jar, Json(customer)))
}

async fn me(CustomerAuth(customer): CustomerAuth) -> Json<Customer> {
    Json(customer)
}

async fn logout(State(s): State<AppState>, jar: CookieJar) -> Result<(CookieJar, Json<Value>)> {
    if let Some(c) = jar.get(CUSTOMER_COOKIE) {
        sqlx::query("UPDATE customer_sessions SET revoked_at = now() WHERE token_hash = $1 AND revoked_at IS NULL")
            .bind(token::hash_token(c.value()))
            .execute(&s.db)
            .await?;
    }
    Ok((
        jar.add(clear_cookie(CUSTOMER_COOKIE, s.cfg.cookie_secure())),
        Json(json!({ "signed_out": true })),
    ))
}

#[cfg(test)]
mod tests {
    use super::clean_name;

    #[test]
    fn names_are_tidied_and_blank_ones_ignored() {
        assert_eq!(
            clean_name(Some("  Priya   Test \n")),
            Some("Priya Test".into())
        );
        assert_eq!(clean_name(Some("   ")), None);
        assert_eq!(clean_name(None), None);
        assert_eq!(
            clean_name(Some(&"x".repeat(200))).map(|n| n.len()),
            Some(80)
        );
    }
}
