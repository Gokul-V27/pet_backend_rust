use axum::{Router, middleware};

use crate::state::AppState;

pub mod admin_auth;
pub mod admin_catalogue;
pub mod admin_images;
pub mod admin_insights;
pub mod admin_orders;
pub mod customer_auth;
pub mod health;
pub mod orders;
pub mod public_catalogue;
mod security;
pub mod webhooks;

/// Header every changing request must carry (see security.rs).
pub const SECURITY_HEADER: &str = security::CLIENT_HEADER;

/// Everything under `/api/v1`.
pub fn api() -> Router<AppState> {
    Router::new()
        .merge(customer_auth::routes())
        .merge(public_catalogue::routes())
        .merge(orders::routes())
        .nest(
            "/admin",
            admin_auth::routes()
                .merge(admin_images::routes())
                .merge(admin_catalogue::routes())
                .merge(admin_orders::routes())
                .merge(admin_insights::routes()),
        )
        // Changing requests must come from our own websites (see security::require_client_header).
        .layer(middleware::from_fn(security::require_client_header))
        // Added after the guard, so it doesn't apply: Razorpay's servers prove themselves with a signature.
        .merge(webhooks::routes())
}
