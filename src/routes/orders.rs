//! Shop orders: price a cart, place an order, see your orders, cancel, confirm a payment.
//! Every route works only on the signed-in customer's own orders.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::auth::CustomerAuth;
use crate::error::{AppError, Result};
use crate::services::orders::{self, Captured, CartLine, OrderView, QuoteRequest};
use crate::services::pricing::{Quote, paise};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/orders/quote", post(quote))
        .route("/orders", get(list).post(place))
        .route("/orders/{number}", get(one))
        .route("/orders/{number}/cancel", post(cancel))
        .route("/payments/razorpay/verify", post(verify))
}

async fn quote(
    State(s): State<AppState>,
    auth: CustomerAuth,
    Json(req): Json<QuoteRequest>,
) -> Result<Json<Quote>> {
    let mut conn = s.db.acquire().await?;
    let (_, q) = orders::quote(&mut conn, Some(auth.0.id), &req, false).await?;
    Ok(Json(q))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Address {
    name: String,
    phone: String,
    line1: String,
    #[serde(default)]
    line2: Option<String>,
    area: String,
    city: String,
    pincode: String,
    #[serde(default)]
    landmark: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Slot {
    date: NaiveDate,
    label: String,
    kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaceOrder {
    lines: Vec<CartLine>,
    address: Address,
    slot: Slot,
    payment: String,
    coupon_code: Option<String>,
    #[serde(default)]
    donate: bool,
    #[serde(default)]
    whatsapp_updates: bool,
    /// The total the customer saw, in rupees. If the server's differs, nothing is charged.
    expected_total: Option<i64>,
}

fn clean(s: &str, max: usize, what: &str) -> Result<String> {
    let t = s.trim();
    if t.is_empty() || t.chars().count() > max {
        return Err(AppError::invalid(format!(
            "Check the {what} (1–{max} characters)."
        )));
    }
    Ok(t.to_owned())
}

fn optional(s: &Option<String>, max: usize, what: &str) -> Result<Option<String>> {
    match s.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => Ok(Some(clean(t, max, what)?)),
        None => Ok(None),
    }
}

/// Checks the address server-side and returns the copy stored with the order.
fn checked_address(a: &Address) -> Result<Value> {
    let phone: String = a.phone.chars().filter(char::is_ascii_digit).collect();
    let phone = phone
        .strip_prefix("91")
        .filter(|p| p.len() == 10)
        .unwrap_or(&phone)
        .to_owned();
    if phone.len() != 10 || !phone.starts_with(['6', '7', '8', '9']) {
        return Err(AppError::invalid(
            "Enter a 10-digit Indian mobile number for delivery.",
        ));
    }
    if orders::serviceable(a.pincode.trim()).is_none() {
        return Err(AppError::invalid(
            "We deliver in and around Chennai for now. Check the pincode.",
        ));
    }
    let label = a
        .label
        .as_deref()
        .filter(|l| matches!(*l, "Home" | "Work" | "Other"))
        .unwrap_or("Home");
    Ok(json!({
        "label": label,
        "name": clean(&a.name, 80, "name")?,
        "phone": phone,
        "line1": clean(&a.line1, 160, "address")?,
        "line2": optional(&a.line2, 160, "address")?,
        "area": clean(&a.area, 80, "area")?,
        "city": clean(&a.city, 60, "city")?,
        "pincode": a.pincode.trim(),
        "landmark": optional(&a.landmark, 120, "landmark")?,
    }))
}

fn idempotency_key(headers: &HeaderMap) -> Result<String> {
    headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| {
            (8..=100).contains(&k.len())
                && k.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
        .map(str::to_owned)
        .ok_or_else(|| AppError::invalid("Missing Idempotency-Key header."))
}

/// A fingerprint of everything the customer chose, so the same Idempotency-Key can only ever mean
/// the same order. (The total they saw is left out: it is checked separately.)
fn request_hash(
    lines: &[CartLine],
    address: &Value,
    req: &PlaceOrder,
    coupon: Option<&str>,
) -> String {
    let lines: Vec<Value> = lines
        .iter()
        .map(|l| {
            json!([
                l.variant_id,
                l.autoship,
                l.qty,
                l.frequency_days.filter(|_| l.autoship)
            ])
        })
        .collect();
    let canonical = json!({
        "lines": lines,
        "address": address,
        "slot": [req.slot.date.to_string(), req.slot.label.trim(), req.slot.kind],
        "payment": req.payment,
        "coupon": coupon,
        "donate": req.donate,
        "whatsapp": req.whatsapp_updates,
    });
    hex::encode(Sha256::digest(canonical.to_string().as_bytes()))
}

/// Places an order. The server prices it, reserves stock and (for online payment) opens a Razorpay
/// order for exactly that amount. Sending the same Idempotency-Key again returns the same order.
async fn place(
    State(s): State<AppState>,
    auth: CustomerAuth,
    headers: HeaderMap,
    Json(req): Json<PlaceOrder>,
) -> Result<Json<OrderView>> {
    let customer_id = auth.0.id;
    let key = idempotency_key(&headers)?;

    // Everything that needs no database first.
    let online = match req.payment.as_str() {
        "cod" => false,
        "upi" | "card" | "netbanking" | "wallet" => true,
        _ => return Err(AppError::invalid("Choose a way to pay.")),
    };
    let address = checked_address(&req.address)?;
    let lines = orders::tidy_lines(&req.lines)?;
    let coupon = req
        .coupon_code
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_uppercase);
    let hash = request_hash(&lines, &address, &req, coupon.as_deref());

    let mut tx = s.db.begin().await?;
    // One checkout per customer at a time: this serialises double taps and two tabs, so the replay
    // check and the first-order and coupon-use counts below never race each other.
    sqlx::query("SELECT 1 FROM customers WHERE id = $1 FOR UPDATE")
        .bind(customer_id)
        .execute(&mut *tx)
        .await?;

    let existing: Option<(Uuid, String, String, Option<String>, i64)> = sqlx::query_as(
        "SELECT id, number, status, request_hash, total_paise FROM orders WHERE customer_id = $1 AND idempotency_key = $2",
    )
    .bind(customer_id)
    .bind(&key)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((order_id, number, status, stored_hash, total_paise)) = existing {
        if stored_hash.as_deref().is_some_and(|h| h != hash) {
            return Err(AppError::IdempotencyKeyReused);
        }
        if matches!(status.as_str(), "payment-failed" | "cancelled") {
            return Err(AppError::OrderClosed);
        }
        // A pending online order whose gateway order was never opened (the request died, or the
        // gateway was down): open it now.
        let needs_gateway: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM payments WHERE order_id = $1 AND status = 'pending' AND gateway_order_id IS NULL",
        )
        .bind(order_id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        if status == "pending" && needs_gateway.is_some() {
            orders::attach_gateway_order(&s, order_id, &number, total_paise).await?;
        }
        let mut conn = s.db.acquire().await?;
        let view = orders::view(&mut conn, &number, Some(customer_id), false).await?;
        return Ok(Json(with_key(&s, view)));
    }

    if online && s.razorpay.is_none() {
        return Err(AppError::PaymentsUnavailable);
    }
    let pincode = address["pincode"].as_str().unwrap_or_default();
    let slot_label = orders::checked_slot_label(
        pincode,
        &req.slot.kind,
        req.slot.date,
        &req.slot.label,
        orders::now_ist(),
    )?;

    // Unpaid checkouts left behind by this customer must not hold stock or count as a first order.
    orders::close_customer_pending(&mut tx, customer_id).await?;

    let qreq = QuoteRequest {
        lines: req.lines,
        coupon_code: coupon,
        delivery: Some(req.slot.kind.clone()),
        payment: Some(req.payment.clone()),
        donate: req.donate,
    };
    let (_, q) = orders::quote(&mut tx, Some(customer_id), &qreq, true).await?;
    if let Some(reason) = &q.coupon_error {
        return Err(AppError::invalid(reason.clone()));
    }
    if !online && !q.cod_allowed {
        return Err(AppError::invalid(
            "Cash on delivery is for orders up to ₹5,000. Please pay online.",
        ));
    }
    if req.expected_total.is_some_and(|t| t != q.total) {
        return Err(AppError::PriceChanged { total: q.total });
    }

    let paid_now = q.total == 0;
    let (status, payment_status) = match (online, paid_now) {
        (_, true) => ("confirmed", "paid"),
        (false, false) => ("confirmed", "cod-due"),
        (true, false) => ("pending", "pending"),
    };
    let (order_id, number) = orders::insert_order(
        &mut tx,
        customer_id,
        &key,
        &q,
        &req.payment,
        status,
        payment_status,
        &address,
        req.slot.date,
        &slot_label,
        &req.slot.kind,
        req.whatsapp_updates,
        &hash,
    )
    .await?;
    orders::take_stock(&mut tx, &q, &number).await?;
    let note = if status == "pending" {
        "Waiting for payment"
    } else {
        "Order placed"
    };
    orders::add_event(
        &mut tx,
        order_id,
        status,
        Some(note),
        "customer",
        Some(customer_id),
    )
    .await?;
    if status == "pending" {
        // The payment row exists from the moment the order does, so an order is never left pending
        // without one (the expiry job closes it either way).
        sqlx::query(
            "INSERT INTO payments (order_id, provider, method, amount_paise, status) VALUES ($1, 'razorpay', $2, $3, 'pending')",
        )
        .bind(order_id)
        .bind(&req.payment)
        .bind(paise(q.total))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    if status == "pending" {
        orders::attach_gateway_order(&s, order_id, &number, paise(q.total)).await?;
    }
    let mut conn = s.db.acquire().await?;
    let view = orders::view(&mut conn, &number, Some(customer_id), false).await?;
    Ok(Json(with_key(&s, view)))
}

/// Adds the public key id the browser needs to open Razorpay Checkout.
fn with_key(s: &AppState, mut view: OrderView) -> OrderView {
    if let (Some(rzp), Some(Value::Object(m))) = (&s.razorpay, view.razorpay.as_mut()) {
        m.insert("keyId".into(), Value::String(rzp.key_id().to_owned()));
    }
    view
}

async fn list(State(s): State<AppState>, auth: CustomerAuth) -> Result<Json<Vec<OrderView>>> {
    let mut conn = s.db.acquire().await?;
    let numbers: Vec<(String,)> = sqlx::query_as(
        "SELECT number FROM orders WHERE customer_id = $1 ORDER BY created_at DESC LIMIT 100",
    )
    .bind(auth.0.id)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::with_capacity(numbers.len());
    for (n,) in numbers {
        out.push(with_key(
            &s,
            orders::view(&mut conn, &n, Some(auth.0.id), false).await?,
        ));
    }
    Ok(Json(out))
}

async fn one(
    State(s): State<AppState>,
    auth: CustomerAuth,
    Path(number): Path<String>,
) -> Result<Json<OrderView>> {
    let mut conn = s.db.acquire().await?;
    let view = orders::view(&mut conn, &number, Some(auth.0.id), false).await?;
    Ok(Json(with_key(&s, view)))
}

/// Cancel before packing. A paid online order is refunded in full straight away.
async fn cancel(
    State(s): State<AppState>,
    auth: CustomerAuth,
    Path(number): Path<String>,
) -> Result<Json<OrderView>> {
    orders::cancel(
        &s,
        &number,
        Some(auth.0.id),
        "customer",
        Some(auth.0.id),
        "Cancelled by customer",
    )
    .await?;
    let mut conn = s.db.acquire().await?;
    Ok(Json(
        orders::view(&mut conn, &number, Some(auth.0.id), false).await?,
    ))
}

#[derive(Deserialize)]
struct Verify {
    razorpay_order_id: String,
    razorpay_payment_id: String,
    razorpay_signature: String,
}

/// Razorpay Checkout's success callback. Only a valid signature for this customer's own order
/// marks it paid; the webhook confirms the same payment again (harmlessly) a moment later.
async fn verify(
    State(s): State<AppState>,
    auth: CustomerAuth,
    Json(req): Json<Verify>,
) -> Result<Json<OrderView>> {
    let rzp = s.razorpay.as_ref().ok_or(AppError::PaymentsUnavailable)?;
    if !rzp.checkout_signature_ok(
        &req.razorpay_order_id,
        &req.razorpay_payment_id,
        &req.razorpay_signature,
    ) {
        return Err(AppError::PaymentNotVerified);
    }
    let mut tx = s.db.begin().await?;
    let number: Option<(String,)> = sqlx::query_as(
        "SELECT o.number FROM payments p JOIN orders o ON o.id = p.order_id
          WHERE p.gateway_order_id = $1 AND o.customer_id = $2",
    )
    .bind(&req.razorpay_order_id)
    .bind(auth.0.id)
    .fetch_optional(&mut *tx)
    .await?;
    let (number,) = number.ok_or(AppError::PaymentNotVerified)?;
    let outcome = orders::capture(
        &mut tx,
        &req.razorpay_order_id,
        &req.razorpay_payment_id,
        None,
    )
    .await?;
    tx.commit().await?;
    match outcome {
        Captured::Rejected => return Err(AppError::PaymentNotVerified),
        Captured::RefundDue(payment_id) => {
            // Paid for an order that can't be fulfilled: the money goes straight back.
            if let Err(e) = orders::refund_detached(
                &s,
                payment_id,
                None,
                "Order could not be fulfilled".into(),
                None,
            )
            .await
            {
                tracing::error!(error = ?e, order = %number, "automatic refund failed; refund it from the admin");
            }
        }
        Captured::Done => {}
    }
    let mut conn = s.db.acquire().await?;
    Ok(Json(
        orders::view(&mut conn, &number, Some(auth.0.id), false).await?,
    ))
}
