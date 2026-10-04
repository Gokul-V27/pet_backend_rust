//! Wagwell API: one server for the shop and the admin website.

pub mod auth;
pub mod catalogue;
pub mod config;
pub mod error;
pub mod models;
pub mod repo;
pub mod routes;
pub mod services;
pub mod state;

use std::time::Duration;

use axum::{
    Router,
    http::{HeaderName, HeaderValue, Method, header},
};
use tower::ServiceBuilder;
use tower_http::{
    compression::CompressionLayer, cors::CorsLayer, limit::RequestBodyLimitLayer,
    set_header::SetResponseHeaderLayer, timeout::TimeoutLayer, trace::TraceLayer,
};

use crate::state::AppState;

pub fn app(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(state.cfg.cors_origins.clone())
        .allow_credentials(true)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([
            header::CONTENT_TYPE,
            HeaderName::from_static(routes::SECURITY_HEADER),
        ])
        .max_age(Duration::from_secs(600));

    Router::new()
        .merge(routes::health::routes())
        .nest("/api/v1", routes::api())
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(TimeoutLayer::with_status_code(
                    axum::http::StatusCode::REQUEST_TIMEOUT,
                    Duration::from_secs(15),
                ))
                // Hard ceiling for any request; normal JSON routes are held to 1 MB by axum's default.
                .layer(RequestBodyLimitLayer::new(
                    services::storage::MAX_IMAGE_BYTES + 128 * 1024,
                ))
                .layer(CompressionLayer::new())
                // API answers are personal: never cached, never sniffed, never framed.
                .layer(SetResponseHeaderLayer::overriding(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("no-store"),
                ))
                .layer(SetResponseHeaderLayer::overriding(
                    header::X_CONTENT_TYPE_OPTIONS,
                    HeaderValue::from_static("nosniff"),
                ))
                .layer(SetResponseHeaderLayer::overriding(
                    header::REFERRER_POLICY,
                    HeaderValue::from_static("no-referrer"),
                ))
                .layer(SetResponseHeaderLayer::overriding(
                    header::X_FRAME_OPTIONS,
                    HeaderValue::from_static("DENY"),
                ))
                .layer(cors),
        )
        .with_state(state)
}
