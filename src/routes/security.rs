//! Cross-site request forgery guard. Browsers only let *our* websites (the CORS allow-list) add a
//! custom header to a cross-site request, so requiring one on every changing request blocks a
//! malicious page from making a signed-in person's browser act for them. Cookies are also SameSite=Lax.

use axum::{
    extract::Request,
    http::Method,
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::error::AppError;

pub const CLIENT_HEADER: &str = "x-wagwell-client";

pub async fn require_client_header(req: Request, next: Next) -> Response {
    let changes_data = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if changes_data
        && req
            .headers()
            .get(CLIENT_HEADER)
            .and_then(|v| v.to_str().ok())
            != Some("web")
    {
        return AppError::Forbidden.into_response();
    }
    next.run(req).await
}
