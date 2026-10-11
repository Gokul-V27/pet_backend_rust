//! Delivery riders (the delivery phone app): sign in with mobile + password, see the orders
//! assigned to you, start a delivery, hand it over with the customer's 4-digit code, and share
//! your position while you're out so the customer can follow you on a map.
//!
//! A rider only ever sees orders assigned to them, and only what's needed at the door: the
//! address, the delivery name and phone, the items and any cash to collect.
//!
//! A rider sets their own password with a one-time setup code the shop gives them
//! (`POST /rider/auth/setup`), so nobody else ever knows it. The same works as a password reset.

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::auth::{RIDER_SESSION_HOURS, RiderAuth};
use crate::error::{AppError, Result};
use crate::repo::audit;
use crate::services::{orders, otp, password, token, totp};
use crate::state::AppState;

const MAX_FAILED_LOGINS: i32 = 5;
const LOCK_MINUTES: i32 = 15;
/// Wrong handover codes allowed per delivery before the shop has to confirm it.
pub const MAX_CODE_ATTEMPTS: i32 = 5;
/// Wrong tries allowed on one setup code before the shop has to issue a new one.
pub const MAX_SETUP_ATTEMPTS: i32 = 5;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/rider/auth/login", post(login))
        .route("/rider/auth/setup", post(setup))
        .route("/rider/auth/me", get(me))
        .route("/rider/auth/logout", post(logout))
        .route("/rider/orders", get(list))
        .route("/rider/orders/{number}/start", post(start))
        .route("/rider/orders/{number}/deliver", post(deliver))
        .route("/rider/orders/{number}/attempt", post(failed_attempt))
        .route("/rider/location", post(location))
}

/* ───────────────────────── Sign-in ───────────────────────── */

#[derive(Deserialize)]
struct Login {
    mobile: String,
    password: String,
}

type RiderRow = (Uuid, String, String, i32, bool, bool);

async fn login(State(s): State<AppState>, Json(req): Json<Login>) -> Result<Json<Value>> {
    let mobile = req.mobile.trim().to_owned();
    if !otp::valid_mobile(&mobile) || req.password.is_empty() || req.password.len() > 256 {
        return Err(AppError::InvalidCredentials);
    }
    let rider: Option<RiderRow> = sqlx::query_as(
        "SELECT id, name, password_hash, failed_attempts, active, COALESCE(locked_until > now(), false)
           FROM riders WHERE mobile = $1",
    )
    .bind(&mobile)
    .fetch_optional(&s.db)
    .await?;

    // Same timing whether or not the number is a rider (see admin_auth.rs).
    let stored = rider
        .as_ref()
        .map(|r| r.2.clone())
        .unwrap_or_else(|| password::dummy_hash().to_owned());
    let supplied = req.password;
    let password_ok = tokio::task::spawn_blocking(move || password::verify(&supplied, &stored))
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("password check failed: {e}")))?;

    let Some((id, name, _hash, _failed, active, locked)) = rider else {
        return Err(AppError::InvalidCredentials);
    };
    if locked {
        return Err(AppError::RateLimited(format!(
            "too many wrong attempts; try again in {LOCK_MINUTES} minutes"
        )));
    }
    if !password_ok || !active {
        sqlx::query(
            "UPDATE riders SET failed_attempts = failed_attempts + 1,
                    locked_until = CASE WHEN failed_attempts + 1 >= $2 THEN now() + ($3::int * interval '1 minute') ELSE locked_until END
              WHERE id = $1",
        )
        .bind(id)
        .bind(MAX_FAILED_LOGINS)
        .bind(LOCK_MINUTES)
        .execute(&s.db)
        .await?;
        audit::record(
            &s.db,
            "rider",
            Some(id),
            "rider.login_failed",
            None,
            json!({ "reason": if active { "bad_password" } else { "inactive" } }),
        )
        .await;
        return Err(AppError::InvalidCredentials);
    }

    start_session(&s, id, &mobile, &name, "password").await
}

/// Signs the rider in on this phone: the token goes back once, in the body.
async fn start_session(
    s: &AppState,
    id: Uuid,
    mobile: &str,
    name: &str,
    via: &str,
) -> Result<Json<Value>> {
    let (session_token, token_hash) = token::new_token()?;
    let mut tx = s.db.begin().await?;
    sqlx::query("UPDATE riders SET failed_attempts = 0, locked_until = NULL, last_login_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO rider_sessions (rider_id, token_hash, expires_at) VALUES ($1, $2, now() + ($3::int * interval '1 hour'))")
        .bind(id)
        .bind(token_hash)
        .bind(RIDER_SESSION_HOURS as i32)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    audit::record(
        &s.db,
        "rider",
        Some(id),
        "rider.login",
        None,
        json!({ "via": via }),
    )
    .await;

    Ok(Json(json!({
        "token": session_token,
        "rider": { "id": id, "mobile": mobile, "name": name },
        "expiresInHours": RIDER_SESSION_HOURS,
    })))
}

#[derive(Deserialize)]
struct Setup {
    mobile: String,
    code: String,
    password: String,
}

/// The rider's first sign-in, or a reset: the one-time code from the shop plus a password of the
/// rider's own choosing. Ends any other sessions and signs this phone in.
async fn setup(State(s): State<AppState>, Json(req): Json<Setup>) -> Result<Json<Value>> {
    let mobile = req.mobile.trim().to_owned();
    if !otp::valid_mobile(&mobile) || req.code.len() > 20 {
        return Err(AppError::OtpInvalid);
    }
    password::check_strength(&req.password).map_err(AppError::invalid)?;

    let mut tx = s.db.begin().await?;
    let row: Option<(Uuid, String, i32, Uuid, String, bool)> = sqlx::query_as(
        "SELECT c.id, c.code_hash, c.attempts, r.id, r.name, r.active
           FROM rider_setup_codes c JOIN riders r ON r.id = c.rider_id
          WHERE r.mobile = $1 AND c.used_at IS NULL AND c.expires_at > now()
          ORDER BY c.created_at DESC LIMIT 1 FOR UPDATE OF c",
    )
    .bind(&mobile)
    .fetch_optional(&mut *tx)
    .await?;
    // One answer for "no such rider", "no code", "used up" and "wrong code".
    let Some((code_id, code_hash, attempts, rider_id, name, active)) = row else {
        return Err(AppError::OtpInvalid);
    };
    if attempts >= MAX_SETUP_ATTEMPTS || !active {
        return Err(AppError::OtpInvalid);
    }
    sqlx::query("UPDATE rider_setup_codes SET attempts = attempts + 1 WHERE id = $1")
        .bind(code_id)
        .execute(&mut *tx)
        .await?;
    let typed = totp::hash_code("rider-setup", &req.code);
    if !bool::from(typed.as_bytes().ct_eq(code_hash.as_bytes())) {
        tx.commit().await?; // keep the spent try
        audit::record(
            &s.db,
            "rider",
            Some(rider_id),
            "rider.setup_failed",
            None,
            json!({}),
        )
        .await;
        return Err(AppError::OtpInvalid);
    }
    let pw = req.password;
    let hash = tokio::task::spawn_blocking(move || password::hash(&pw))
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("hashing failed: {e}")))??;
    sqlx::query("UPDATE riders SET password_hash = $2, failed_attempts = 0, locked_until = NULL WHERE id = $1")
        .bind(rider_id)
        .bind(hash)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE rider_setup_codes SET used_at = now() WHERE id = $1")
        .bind(code_id)
        .execute(&mut *tx)
        .await?;
    // A reset signs out every other phone.
    sqlx::query(
        "UPDATE rider_sessions SET revoked_at = now() WHERE rider_id = $1 AND revoked_at IS NULL",
    )
    .bind(rider_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    audit::record(
        &s.db,
        "rider",
        Some(rider_id),
        "rider.password_set",
        None,
        json!({}),
    )
    .await;
    start_session(&s, rider_id, &mobile, &name, "setup_code").await
}

async fn me(rider: RiderAuth) -> Json<RiderAuth> {
    Json(rider)
}

async fn logout(State(s): State<AppState>, rider: RiderAuth) -> Result<Json<Value>> {
    sqlx::query("UPDATE rider_sessions SET revoked_at = now() WHERE id = $1")
        .bind(rider.session_id)
        .execute(&s.db)
        .await?;
    // Off shift: stop showing a position to anyone.
    sqlx::query("DELETE FROM rider_locations WHERE rider_id = $1")
        .bind(rider.id)
        .execute(&s.db)
        .await?;
    audit::record(
        &s.db,
        "rider",
        Some(rider.id),
        "rider.logout",
        None,
        json!({}),
    )
    .await;
    Ok(Json(json!({ "signed_out": true })))
}

/* ───────────────────────── Stops ───────────────────────── */

type StopRow = (
    String,
    String,
    String,
    String,
    Value,
    chrono::NaiveDate,
    String,
    i64,
    Option<chrono::DateTime<chrono::Utc>>,
    Value,
);

/// Orders assigned to me that are still to deliver, plus what I delivered today.
async fn list(State(s): State<AppState>, rider: RiderAuth) -> Result<Json<Vec<Value>>> {
    let rows: Vec<StopRow> = sqlx::query_as(
        "SELECT o.number, o.status, o.payment_method, o.payment_status, o.address, o.slot_date, o.slot_label,
                o.total_paise,
                (SELECT max(e.at) FROM order_events e WHERE e.order_id = o.id AND e.status = 'delivered'),
                COALESCE((SELECT json_agg(json_build_object('name', i.name, 'size', i.size, 'qty', i.qty) ORDER BY i.id)
                            FROM order_items i WHERE i.order_id = o.id), '[]'::json)
           FROM orders o
          WHERE o.rider_id = $1
            AND (o.status IN ('packed', 'shipped', 'out-for-delivery')
                 OR (o.status = 'delivered' AND o.updated_at > date_trunc('day', now() AT TIME ZONE 'Asia/Kolkata') AT TIME ZONE 'Asia/Kolkata'))
          ORDER BY (o.status = 'delivered'), o.slot_date, o.created_at",
    )
    .bind(rider.id)
    .fetch_all(&s.db)
    .await?;
    Ok(Json(rows.into_iter().map(stop_json).collect()))
}

fn stop_json(r: StopRow) -> Value {
    let (
        number,
        status,
        method,
        payment_status,
        address,
        slot_date,
        slot_label,
        total,
        delivered_at,
        items,
    ) = r;
    let cash_due = payment_status == "cod-due";
    json!({
        "number": number,
        "status": status,
        "payment": method,
        // Rupees to collect at the door (0 when already paid online).
        "collect": if cash_due { total / 100 } else { 0 },
        "total": total / 100,
        "slot": { "date": slot_date, "label": slot_label },
        "address": address,
        "items": items,
        "deliveredAt": delivered_at,
    })
}

/// Locks one of *my* orders for a change; anyone else's looks missing.
async fn lock_my_order(
    tx: &mut sqlx::PgConnection,
    number: &str,
    rider_id: Uuid,
) -> Result<(Uuid, String, Option<String>, i32)> {
    sqlx::query_as(
        "SELECT id, status, delivery_code, delivery_code_attempts FROM orders
          WHERE number = $1 AND rider_id = $2 FOR UPDATE",
    )
    .bind(number)
    .bind(rider_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)
}

/// A fresh 4-digit handover code (uniform, from the OS random source).
fn new_delivery_code() -> Result<String> {
    loop {
        let mut b = [0u8; 2];
        getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("no secure random source: {e}"))?;
        let n = u16::from_be_bytes(b);
        if n < 60_000 {
            return Ok(format!("{:04}", n % 10_000));
        }
    }
}

/// Leaving with the order: it goes out for delivery and the customer gets a handover code.
async fn start(
    State(s): State<AppState>,
    rider: RiderAuth,
    Path(number): Path<String>,
) -> Result<Json<Value>> {
    let mut tx = s.db.begin().await?;
    let (order_id, status, _, _) = lock_my_order(&mut tx, &number, rider.id).await?;
    if status == "out-for-delivery" {
        tx.commit().await?;
        return one(&s, &number, rider.id).await;
    }
    if !matches!(status.as_str(), "packed" | "shipped") {
        return Err(AppError::invalid(format!(
            "An order that is {status} can't go out for delivery."
        )));
    }
    sqlx::query(
        "UPDATE orders SET status = 'out-for-delivery', delivery_code = $2, delivery_code_attempts = 0, updated_at = now()
          WHERE id = $1",
    )
    .bind(order_id)
    .bind(new_delivery_code()?)
    .execute(&mut *tx)
    .await?;
    let note = format!("Out for delivery with {}", rider.name);
    orders::add_event(
        &mut tx,
        order_id,
        "out-for-delivery",
        Some(&note),
        "rider",
        Some(rider.id),
    )
    .await?;
    tx.commit().await?;
    audit::record(
        &s.db,
        "rider",
        Some(rider.id),
        "order.out_for_delivery",
        Some(&number),
        json!({}),
    )
    .await;
    one(&s, &number, rider.id).await
}

#[derive(Deserialize)]
struct Deliver {
    code: String,
}

/// Handing over: the customer's code proves the right person got it. Cash on delivery is booked
/// as collected and the sale is posted to the ledger.
async fn deliver(
    State(s): State<AppState>,
    rider: RiderAuth,
    Path(number): Path<String>,
    Json(req): Json<Deliver>,
) -> Result<Json<Value>> {
    let code = req.code.trim();
    if code.len() != 4 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AppError::invalid(
            "Enter the 4-digit code the customer has.",
        ));
    }
    let mut tx = s.db.begin().await?;
    let (order_id, status, stored, attempts) = lock_my_order(&mut tx, &number, rider.id).await?;
    if status != "out-for-delivery" {
        return Err(AppError::invalid("Start this delivery first."));
    }
    let Some(stored) = stored else {
        return Err(AppError::invalid("Start this delivery first."));
    };
    if attempts >= MAX_CODE_ATTEMPTS {
        return Err(AppError::RateLimited(
            "too many wrong codes; call the shop to confirm this delivery".into(),
        ));
    }
    if !bool::from(stored.as_bytes().ct_eq(code.as_bytes())) {
        sqlx::query(
            "UPDATE orders SET delivery_code_attempts = delivery_code_attempts + 1 WHERE id = $1",
        )
        .bind(order_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?; // keep the failed try on record
        let left = MAX_CODE_ATTEMPTS - attempts - 1;
        return Err(AppError::invalid(format!(
            "That code doesn't match. {left} {} left.",
            if left == 1 { "try" } else { "tries" }
        )));
    }
    sqlx::query(
        "UPDATE orders SET status = 'delivered', delivery_code = NULL, updated_at = now() WHERE id = $1",
    )
    .bind(order_id)
    .execute(&mut *tx)
    .await?;
    let note = format!("Delivered by {}", rider.name);
    orders::add_event(
        &mut tx,
        order_id,
        "delivered",
        Some(&note),
        "rider",
        Some(rider.id),
    )
    .await?;
    orders::on_delivered(&mut tx, order_id, &number).await?;
    tx.commit().await?;
    audit::record(
        &s.db,
        "rider",
        Some(rider.id),
        "order.delivered",
        Some(&number),
        json!({}),
    )
    .await;
    one(&s, &number, rider.id).await
}

#[derive(Deserialize)]
struct Attempt {
    reason: String,
}

/// Couldn't hand it over (nobody home, wrong address…): noted on the order for the shop and the
/// customer. The order stays with the rider; the shop decides what happens next.
async fn failed_attempt(
    State(s): State<AppState>,
    rider: RiderAuth,
    Path(number): Path<String>,
    Json(req): Json<Attempt>,
) -> Result<Json<Value>> {
    let reason: String = req
        .reason
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    if reason.is_empty() {
        return Err(AppError::invalid("Say why it couldn't be delivered."));
    }
    let mut tx = s.db.begin().await?;
    let (order_id, status, _, _) = lock_my_order(&mut tx, &number, rider.id).await?;
    if status != "out-for-delivery" {
        return Err(AppError::invalid(
            "Only an order out for delivery can have a failed attempt.",
        ));
    }
    let note = format!("Delivery attempt failed: {reason}");
    orders::add_event(
        &mut tx,
        order_id,
        "out-for-delivery",
        Some(&note),
        "rider",
        Some(rider.id),
    )
    .await?;
    tx.commit().await?;
    audit::record(
        &s.db,
        "rider",
        Some(rider.id),
        "order.delivery_failed",
        Some(&number),
        json!({ "reason": reason }),
    )
    .await;
    one(&s, &number, rider.id).await
}

async fn one(s: &AppState, number: &str, rider_id: Uuid) -> Result<Json<Value>> {
    let row: StopRow = sqlx::query_as(
        "SELECT o.number, o.status, o.payment_method, o.payment_status, o.address, o.slot_date, o.slot_label,
                o.total_paise,
                (SELECT max(e.at) FROM order_events e WHERE e.order_id = o.id AND e.status = 'delivered'),
                COALESCE((SELECT json_agg(json_build_object('name', i.name, 'size', i.size, 'qty', i.qty) ORDER BY i.id)
                            FROM order_items i WHERE i.order_id = o.id), '[]'::json)
           FROM orders o WHERE o.number = $1 AND o.rider_id = $2",
    )
    .bind(number)
    .bind(rider_id)
    .fetch_optional(&s.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(stop_json(row)))
}

/* ───────────────────────── Location ───────────────────────── */

#[derive(Deserialize)]
struct Position {
    lat: f64,
    lng: f64,
    #[serde(default)]
    accuracy: Option<f32>,
    #[serde(default)]
    heading: Option<f32>,
    #[serde(default)]
    speed: Option<f32>,
}

/// Checks a reported position; optional extras that are out of range are dropped, not refused.
/// `(lat, lng, accuracy, heading, speed)`.
type CheckedPosition = (f64, f64, Option<f32>, Option<f32>, Option<f32>);

fn checked_position(p: &Position) -> Result<CheckedPosition> {
    let in_range = p.lat.is_finite()
        && p.lng.is_finite()
        && (-90.0..=90.0).contains(&p.lat)
        && (-180.0..=180.0).contains(&p.lng);
    if !in_range {
        return Err(AppError::invalid("That position isn't valid."));
    }
    let keep = |v: Option<f32>, max: f32| v.filter(|x| x.is_finite() && *x >= 0.0 && *x < max);
    Ok((
        p.lat,
        p.lng,
        keep(p.accuracy, 100_000.0),
        keep(p.heading, 360.0),
        keep(p.speed, 100.0),
    ))
}

/// The app reports where the rider is every few seconds while on duty. Only the latest position
/// is kept, and customers only see it while their own order is out for delivery with this rider.
async fn location(
    State(s): State<AppState>,
    rider: RiderAuth,
    Json(p): Json<Position>,
) -> Result<Json<Value>> {
    let (lat, lng, accuracy, heading, speed) = checked_position(&p)?;
    sqlx::query(
        "INSERT INTO rider_locations (rider_id, lat, lng, accuracy_m, heading, speed_mps, at)
         VALUES ($1, $2, $3, $4, $5, $6, now())
         ON CONFLICT (rider_id) DO UPDATE
            SET lat = $2, lng = $3, accuracy_m = $4, heading = $5, speed_mps = $6, at = now()",
    )
    .bind(rider.id)
    .bind(lat)
    .bind(lng)
    .bind(accuracy)
    .bind(heading)
    .bind(speed)
    .execute(&s.db)
    .await?;
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_codes_are_four_digits() {
        for _ in 0..200 {
            let c = new_delivery_code().unwrap();
            assert_eq!(c.len(), 4);
            assert!(c.bytes().all(|b| b.is_ascii_digit()));
        }
    }

    #[test]
    fn positions_are_checked() {
        let p = |lat, lng| Position {
            lat,
            lng,
            accuracy: Some(12.0),
            heading: Some(400.0),
            speed: Some(f32::NAN),
        };
        let (lat, lng, acc, heading, speed) = checked_position(&p(13.08, 80.27)).unwrap();
        assert_eq!((lat, lng, acc), (13.08, 80.27, Some(12.0)));
        assert_eq!((heading, speed), (None, None));
        assert!(checked_position(&p(91.0, 80.0)).is_err());
        assert!(checked_position(&p(f64::NAN, 80.0)).is_err());
    }
}
