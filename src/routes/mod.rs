use axum::{Router, middleware};

use crate::state::AppState;

pub mod admin_auth;
pub mod admin_catalogue;
pub mod admin_images;
pub mod customer_auth;
pub mod health;
pub mod public_catalogue;
mod security;

/// Header every changing request must carry (see security.rs).
pub const SECURITY_HEADER: &str = security::CLIENT_HEADER;

/// Everything under `/api/v1`.
pub fn api() -> Router<AppState> {
    Router::new()
        .merge(customer_auth::routes())
        .merge(public_catalogue::routes())
        .nest(
            "/admin",
            admin_auth::routes()
                .merge(admin_images::routes())
                .merge(admin_catalogue::routes()),
        )
        // Changing requests must come from our own websites (see security::require_client_header).
        .layer(middleware::from_fn(security::require_client_header))
}
