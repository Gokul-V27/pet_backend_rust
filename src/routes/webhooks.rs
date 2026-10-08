//! Razorpay webhooks. Not behind the website-header guard (Razorpay's servers can't send it);
//! instead every call must carry a valid HMAC of the raw body. Each event id is handled once,
//! so retries and replays change nothing.

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde_json::Value;
use uuid::Uuid;

use crate::error::{AppError, Result};
use crate::services::orders::{self, Captured};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/payments/razorpay/webhook", post(razorpay))
}

async fn razorpay(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode> {
    let rzp = s.razorpay.as_ref().ok_or(AppError::NotFound)?;
    let signature = headers
        .get("x-razorpay-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !rzp.webhook_signature_ok(&body, signature) {
        tracing::warn!("razorpay webhook with a bad signature refused");
        return Err(AppError::Unauthorized);
    }
    let event: Value =
        serde_json::from_slice(&body).map_err(|_| AppError::invalid("bad webhook body"))?;
    let kind = event["event"].as_str().unwrap_or_default().to_owned();
    // Razorpay's per-delivery id; fall back to a hash of the body so a replay is still recognised.
    let event_id = headers
        .get("x-razorpay-event-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| crate::services::token::hash_token(&String::from_utf8_lossy(&body)));

    let mut tx = s.db.begin().await?;
    let fresh = sqlx::query(
        "INSERT INTO webhook_events (id, provider, event) VALUES ($1, 'razorpay', $2) ON CONFLICT DO NOTHING",
    )
    .bind(&event_id)
    .bind(&kind)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if !fresh {
        return Ok(StatusCode::OK);
    }

    let payment = &event["payload"]["payment"]["entity"];
    let refund = &event["payload"]["refund"]["entity"];
    let mut refund_due: Option<Uuid> = None;
    match kind.as_str() {
        "payment.captured" => {
            if let (Some(order_id), Some(payment_id), Some(amount)) = (
                payment["order_id"].as_str(),
                payment["id"].as_str(),
                payment["amount"].as_i64(),
            ) && let Captured::RefundDue(id) =
                orders::capture(&mut tx, order_id, payment_id, Some(amount)).await?
            {
                refund_due = Some(id);
            }
        }
        "payment.failed" => {
            // One failed attempt does not end the checkout: the customer can retry in the same
            // Razorpay window. Only the 30-minute expiry closes an unpaid order. Just note it.
            if let Some(order_id) = payment["order_id"].as_str() {
                let found: Option<(Uuid,)> = sqlx::query_as(
                    "SELECT o.id FROM payments p JOIN orders o ON o.id = p.order_id
                      WHERE p.gateway_order_id = $1 AND o.status = 'pending'",
                )
                .bind(order_id)
                .fetch_optional(&mut *tx)
                .await?;
                if let Some((id,)) = found {
                    orders::add_event(
                        &mut tx,
                        id,
                        "pending",
                        Some("A payment attempt failed"),
                        "gateway",
                        None,
                    )
                    .await?;
                }
            }
        }
        "refund.processed" => {
            if let Some(refund_id) = refund["id"].as_str() {
                orders::refund_processed(&mut tx, refund_id).await?;
            }
        }
        "refund.failed" => {
            if let Some(refund_id) = refund["id"].as_str() {
                orders::refund_failed(&mut tx, refund_id).await?;
            }
        }
        _ => {}
    }
    tx.commit().await?;

    // Paid for an order that can't be fulfilled: send the money straight back. A failure here is
    // logged; the order shows as cancelled and paid, so staff can refund it from the admin.
    if let Some(payment_id) = refund_due
        && let Err(e) = orders::refund_detached(
            &s,
            payment_id,
            None,
            "Order could not be fulfilled".into(),
            None,
        )
        .await
    {
        tracing::error!(error = ?e, payment = %payment_id, "automatic refund failed; refund it from the admin");
    }
    Ok(StatusCode::OK)
}
