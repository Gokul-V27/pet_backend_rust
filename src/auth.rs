//! Who is calling? Sessions are random tokens in HttpOnly cookies (JavaScript can't read them),
//! looked up by hash. Shop customers and staff use different cookies and different tables, so
//! signing in to one never signs you in to the other.
//!
//! The phone apps can't rely on cookies, so they send the same token as `Authorization: Bearer …`.
//! Delivery riders only ever use the bearer form and have their own table (see migration 0009).

use std::str::FromStr;

use axum::extract::{FromRequestParts, OriginalUri};
use axum::http::{HeaderMap, header, request::Parts};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::Serialize;
use uuid::Uuid;

use crate::error::AppError;
use crate::models::{AdminRole, Customer};
use crate::services::token::hash_token;
use crate::state::AppState;

pub const CUSTOMER_COOKIE: &str = "wagwell_session";
pub const ADMIN_COOKIE: &str = "wagwell_admin";

pub const CUSTOMER_SESSION_DAYS: i64 = 30;
/// A rider stays signed in for a long shift; the app signs in again the next day.
pub const RIDER_SESSION_HOURS: i64 = 14;
/// Staff website sessions end after 8 hours, or after 30 minutes without activity.
pub const ADMIN_SESSION_HOURS: i64 = 8;
pub const ADMIN_IDLE_MINUTES: i64 = 30;
/// A staff phone-app session lasts one shift with no idle limit: the shop phone is protected by its
/// own screen lock, and signing in again between every delivery that arrives would slow goods-in down.
pub const ADMIN_MOBILE_SESSION_HOURS: i64 = 12;

pub fn session_cookie(
    name: &'static str,
    value: String,
    secure: bool,
    max_age: time::Duration,
) -> Cookie<'static> {
    Cookie::build((name, value))
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(max_age)
        .build()
}

/// An expired empty cookie, which tells the browser to delete it.
pub fn clear_cookie(name: &'static str, secure: bool) -> Cookie<'static> {
    session_cookie(name, String::new(), secure, time::Duration::ZERO)
}

/// The token from `Authorization: Bearer …`, if the request carries one.
pub fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    // Our tokens are 64 hex characters; anything else can't be a session.
    (token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())).then(|| token.to_owned())
}

/// The session token: a bearer token (phone apps) or else the named cookie (websites).
pub fn session_token(headers: &HeaderMap, cookie: &str) -> Option<String> {
    bearer_token(headers).or_else(|| {
        CookieJar::from_headers(headers)
            .get(cookie)
            .map(|c| c.value().to_owned())
    })
}

/// A signed-in shop customer.
#[derive(Debug, Clone)]
pub struct CustomerAuth(pub Customer);

impl FromRequestParts<AppState> for CustomerAuth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let token = session_token(&parts.headers, CUSTOMER_COOKIE).ok_or(AppError::Unauthorized)?;
        let customer = sqlx::query_as::<_, Customer>(
            "SELECT c.id, c.mobile, c.name, c.email
               FROM customer_sessions s JOIN customers c ON c.id = s.customer_id
              WHERE s.token_hash = $1 AND s.revoked_at IS NULL AND s.expires_at > now()",
        )
        .bind(hash_token(&token))
        .fetch_optional(&state.db)
        .await?;
        customer.map(CustomerAuth).ok_or(AppError::Unauthorized)
    }
}

/// A signed-in staff member.
#[derive(Debug, Clone, Serialize)]
pub struct AdminAuth {
    pub id: Uuid,
    pub email: String,
    pub name: String,
    pub role: AdminRole,
    /// Two-step sign-in (authenticator codes) is on for this account.
    #[serde(rename = "mfaEnabled")]
    pub mfa_enabled: bool,
    /// This role must turn two-step sign-in on before doing anything else.
    #[serde(rename = "mfaSetupRequired")]
    pub mfa_setup_required: bool,
    #[serde(skip)]
    pub session_id: Uuid,
}

/// What a Super Admin or Admin without two-step sign-in may still reach: who they are, signing
/// out, setting it up, and their signed-in devices.
fn allowed_before_mfa_setup(path: &str) -> bool {
    const OK: [&str; 5] = [
        "/admin/auth/me",
        "/admin/auth/logout",
        "/admin/auth/mfa",
        "/admin/auth/mfa/setup",
        "/admin/auth/mfa/enable",
    ];
    OK.iter().any(|p| path.ends_with(p)) || path.contains("/admin/auth/sessions")
}

impl FromRequestParts<AppState> for AdminAuth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let token = session_token(&parts.headers, ADMIN_COOKIE).ok_or(AppError::Unauthorized)?;
        // Checks the session and refreshes its "last seen" time in one step.
        let row: Option<(Uuid, String, String, String, Uuid, bool)> = sqlx::query_as(
            "UPDATE admin_sessions s SET last_seen_at = now()
               FROM admin_users a
              WHERE s.admin_id = a.id AND s.token_hash = $1 AND s.revoked_at IS NULL
                AND s.expires_at > now()
                AND (s.client = 'mobile' OR s.last_seen_at > now() - ($2::int * interval '1 minute'))
                AND a.active
          RETURNING a.id, a.email, a.name, a.role, s.id, a.mfa_enabled_at IS NOT NULL",
        )
        .bind(hash_token(&token))
        .bind(ADMIN_IDLE_MINUTES as i32)
        .fetch_optional(&state.db)
        .await?;
        let (id, email, name, role, session_id, mfa_enabled) = row.ok_or(AppError::Unauthorized)?;
        let role = AdminRole::from_str(&role).map_err(|_| AppError::Unauthorized)?;
        let mfa_setup_required =
            state.cfg.staff_mfa_required && role.must_use_mfa() && !mfa_enabled;
        if mfa_setup_required {
            // Nested routers see a shortened path; the original one is what was asked for.
            let path = parts
                .extensions
                .get::<OriginalUri>()
                .map_or_else(|| parts.uri.path().to_owned(), |u| u.path().to_owned());
            if !allowed_before_mfa_setup(&path) {
                return Err(AppError::MfaSetupRequired);
            }
        }
        Ok(AdminAuth {
            id,
            email,
            name,
            role,
            mfa_enabled,
            mfa_setup_required,
            session_id,
        })
    }
}

/// A signed-in delivery rider (phone app, bearer token only).
#[derive(Debug, Clone, Serialize)]
pub struct RiderAuth {
    pub id: Uuid,
    pub mobile: String,
    pub name: String,
    #[serde(skip)]
    pub session_id: Uuid,
}

impl FromRequestParts<AppState> for RiderAuth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let token = bearer_token(&parts.headers).ok_or(AppError::Unauthorized)?;
        let row: Option<(Uuid, String, String, Uuid)> = sqlx::query_as(
            "SELECT r.id, r.mobile, r.name, s.id
               FROM rider_sessions s JOIN riders r ON r.id = s.rider_id
              WHERE s.token_hash = $1 AND s.revoked_at IS NULL AND s.expires_at > now() AND r.active",
        )
        .bind(hash_token(&token))
        .fetch_optional(&state.db)
        .await?;
        let (id, mobile, name, session_id) = row.ok_or(AppError::Unauthorized)?;
        Ok(RiderAuth {
            id,
            mobile,
            name,
            session_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_tokens_must_look_like_ours() {
        let mut h = HeaderMap::new();
        let good = "a".repeat(64);
        h.insert(
            header::AUTHORIZATION,
            format!("Bearer {good}").parse().unwrap(),
        );
        assert_eq!(bearer_token(&h), Some(good.clone()));
        h.insert(header::AUTHORIZATION, "Bearer short".parse().unwrap());
        assert_eq!(bearer_token(&h), None);
        h.insert(
            header::AUTHORIZATION,
            format!("Basic {good}").parse().unwrap(),
        );
        assert_eq!(bearer_token(&h), None);
    }

    #[test]
    fn bearer_wins_over_cookie() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            "wagwell_session=fromcookie".parse().unwrap(),
        );
        assert_eq!(
            session_token(&h, CUSTOMER_COOKIE).as_deref(),
            Some("fromcookie")
        );
        let bearer = "b".repeat(64);
        h.insert(
            header::AUTHORIZATION,
            format!("Bearer {bearer}").parse().unwrap(),
        );
        assert_eq!(session_token(&h, CUSTOMER_COOKIE), Some(bearer));
    }

    #[test]
    fn cookies_are_http_only_and_lax() {
        let c = session_cookie(CUSTOMER_COOKIE, "abc".into(), true, time::Duration::days(1));
        let s = c.to_string();
        assert!(
            s.contains("HttpOnly")
                && s.contains("Secure")
                && s.contains("SameSite=Lax")
                && s.contains("Path=/")
        );
        let dev = session_cookie(
            CUSTOMER_COOKIE,
            "abc".into(),
            false,
            time::Duration::days(1),
        )
        .to_string();
        assert!(!dev.contains("Secure"));
    }

    #[test]
    fn cleared_cookie_expires_now() {
        let s = clear_cookie(ADMIN_COOKIE, true).to_string();
        assert!(s.contains("Max-Age=0"));
    }
}
