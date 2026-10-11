//! Cross-site request forgery guard. Browsers only let *our* websites (the CORS allow-list) add a
//! custom header to a cross-site request, so requiring one on every changing request blocks a
//! malicious page from making a signed-in person's browser act for them. Cookies are also SameSite=Lax.
//! The phone apps send `mobile` and sign in with a bearer token instead of a cookie; a browser never
//! attaches that token by itself, so it can't be forged cross-site either.

use axum::{
    extract::Request,
    http::Method,
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::error::AppError;

pub const CLIENT_HEADER: &str = "x-wagwell-client";

/// The value the phone apps send. They get their session token in the sign-in response body.
pub const MOBILE_CLIENT: &str = "mobile";

/// True when the request comes from one of the phone apps.
pub fn is_mobile(headers: &axum::http::HeaderMap) -> bool {
    headers.get(CLIENT_HEADER).and_then(|v| v.to_str().ok()) == Some(MOBILE_CLIENT)
}

pub async fn require_client_header(req: Request, next: Next) -> Response {
    let changes_data = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let client = req
        .headers()
        .get(CLIENT_HEADER)
        .and_then(|v| v.to_str().ok());
    if changes_data && !matches!(client, Some("web" | MOBILE_CLIENT)) {
        return AppError::Forbidden.into_response();
    }
    next.run(req).await
}
