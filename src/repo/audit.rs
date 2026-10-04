use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

/// Appends to the audit log. A failure to write is logged but never blocks the action itself.
pub async fn record(
    db: &PgPool,
    actor_type: &str,
    actor_id: Option<Uuid>,
    action: &str,
    target: Option<&str>,
    detail: Value,
) {
    let res = sqlx::query("INSERT INTO audit_log (actor_type, actor_id, action, target, detail) VALUES ($1, $2, $3, $4, $5)")
        .bind(actor_type)
        .bind(actor_id)
        .bind(action)
        .bind(target)
        .bind(detail)
        .execute(db)
        .await;
    if let Err(e) = res {
        tracing::error!(error = ?e, action, "could not write audit log");
    }
}
