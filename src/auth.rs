//! Who is calling? Sessions are random tokens in HttpOnly cookies (JavaScript can't read them),
//! looked up by hash. Shop customers and staff use different cookies and different tables, so
//! signing in to one never signs you in to the other.

use std::str::FromStr;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
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
/// Staff sessions end after 8 hours, or after 30 minutes without activity.
pub const ADMIN_SESSION_HOURS: i64 = 8;
pub const ADMIN_IDLE_MINUTES: i64 = 30;

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

/// A signed-in shop customer.
#[derive(Debug, Clone)]
pub struct CustomerAuth(pub Customer);

impl FromRequestParts<AppState> for CustomerAuth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let jar = CookieJar::from_headers(&parts.headers);
        let token = jar
            .get(CUSTOMER_COOKIE)
            .ok_or(AppError::Unauthorized)?
            .value()
            .to_owned();
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
    #[serde(skip)]
    pub session_id: Uuid,
}

impl FromRequestParts<AppState> for AdminAuth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let jar = CookieJar::from_headers(&parts.headers);
        let token = jar
            .get(ADMIN_COOKIE)
            .ok_or(AppError::Unauthorized)?
            .value()
            .to_owned();
        // Checks the session and refreshes its "last seen" time in one step.
        let row: Option<(Uuid, String, String, String, Uuid)> = sqlx::query_as(
            "UPDATE admin_sessions s SET last_seen_at = now()
               FROM admin_users a
              WHERE s.admin_id = a.id AND s.token_hash = $1 AND s.revoked_at IS NULL
                AND s.expires_at > now() AND s.last_seen_at > now() - ($2::int * interval '1 minute')
                AND a.active
          RETURNING a.id, a.email, a.name, a.role, s.id",
        )
        .bind(hash_token(&token))
        .bind(ADMIN_IDLE_MINUTES as i32)
        .fetch_optional(&state.db)
        .await?;
        let (id, email, name, role, session_id) = row.ok_or(AppError::Unauthorized)?;
        let role = AdminRole::from_str(&role).map_err(|_| AppError::Unauthorized)?;
        Ok(AdminAuth {
            id,
            email,
            name,
            role,
            session_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
