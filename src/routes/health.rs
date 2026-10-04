use axum::{Json, Router, extract::State, routing::get};
use serde_json::{Value, json};

use crate::error::Result;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
}

/// The process is up. Used by the host to know whether to restart it.
async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// The process can reach its database.
async fn ready(State(s): State<AppState>) -> Result<Json<Value>> {
    sqlx::query("SELECT 1").execute(&s.db).await?;
    Ok(Json(json!({ "status": "ready" })))
}
