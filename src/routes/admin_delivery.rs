//! Staff: the delivery riders (add, switch off, see where they are) and which rider carries an order.
//! Order managers assign riders; only Super Admin and Admin add or switch off rider accounts.

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::AdminAuth;
use crate::error::{AppError, Result};
use crate::repo::audit;
use crate::services::{otp, password};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/riders", get(list).post(create))
        .route("/riders/{id}/active", post(set_active))
        .route("/orders/{number}/rider", post(assign))
}

type RiderListRow = (
    Uuid,
    String,
    String,
    bool,
    Option<chrono::DateTime<chrono::Utc>>,
    i64,
    Option<f64>,
    Option<f64>,
    Option<chrono::DateTime<chrono::Utc>>,
);

async fn list(State(s): State<AppState>, admin: AdminAuth) -> Result<Json<Vec<Value>>> {
    if !admin.role.can_manage_orders() {
        return Err(AppError::Forbidden);
    }
    let rows: Vec<RiderListRow> = sqlx::query_as(
        "SELECT r.id, r.mobile, r.name, r.active, r.last_login_at,
                (SELECT count(*) FROM orders o WHERE o.rider_id = r.id AND o.status IN ('packed', 'shipped', 'out-for-delivery')),
                l.lat, l.lng, l.at
           FROM riders r LEFT JOIN rider_locations l ON l.rider_id = r.id
          ORDER BY r.active DESC, r.name",
    )
    .fetch_all(&s.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(
                |(id, mobile, name, active, last_login, open, lat, lng, at)| {
                    json!({
                        "id": id, "mobile": mobile, "name": name, "active": active,
                        "lastLoginAt": last_login, "openOrders": open,
                        "location": at.map(|at| json!({ "lat": lat, "lng": lng, "at": at })),
                    })
                },
            )
            .collect(),
    ))
}

#[derive(Deserialize)]
struct NewRider {
    mobile: String,
    name: String,
    password: String,
}

async fn create(
    State(s): State<AppState>,
    admin: AdminAuth,
    Json(req): Json<NewRider>,
) -> Result<Json<Value>> {
    if !admin.role.can_manage_riders() {
        return Err(AppError::Forbidden);
    }
    let mobile = req.mobile.trim();
    if !otp::valid_mobile(mobile) {
        return Err(AppError::invalid(
            "Enter the rider's 10-digit mobile number.",
        ));
    }
    let name: String = req
        .name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect();
    if name.is_empty() {
        return Err(AppError::invalid("Enter the rider's name."));
    }
    password::check_strength(&req.password).map_err(AppError::invalid)?;
    let pw = req.password;
    let hash = tokio::task::spawn_blocking(move || password::hash(&pw))
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("hashing failed: {e}")))??;
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO riders (mobile, name, password_hash) VALUES ($1, $2, $3)
         ON CONFLICT (mobile) DO NOTHING RETURNING id",
    )
    .bind(mobile)
    .bind(&name)
    .bind(hash)
    .fetch_optional(&s.db)
    .await?;
    let id =
        id.ok_or_else(|| AppError::invalid("A rider with that mobile number already exists."))?;
    audit::record(
        &s.db,
        "admin",
        Some(admin.id),
        "rider.create",
        Some(&id.to_string()),
        json!({ "mobile": mobile }),
    )
    .await;
    Ok(Json(
        json!({ "id": id, "mobile": mobile, "name": name, "active": true }),
    ))
}

#[derive(Deserialize)]
struct Active {
    active: bool,
}

/// Switching a rider off signs them out everywhere and hides their position.
async fn set_active(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(id): Path<Uuid>,
    Json(req): Json<Active>,
) -> Result<Json<Value>> {
    if !admin.role.can_manage_riders() {
        return Err(AppError::Forbidden);
    }
    let mut tx = s.db.begin().await?;
    let updated = sqlx::query(
        "UPDATE riders SET active = $2, failed_attempts = 0, locked_until = NULL WHERE id = $1",
    )
    .bind(id)
    .bind(req.active)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(AppError::NotFound);
    }
    if !req.active {
        sqlx::query("UPDATE rider_sessions SET revoked_at = now() WHERE rider_id = $1 AND revoked_at IS NULL")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM rider_locations WHERE rider_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    audit::record(
        &s.db,
        "admin",
        Some(admin.id),
        "rider.active",
        Some(&id.to_string()),
        json!({ "active": req.active }),
    )
    .await;
    Ok(Json(json!({ "id": id, "active": req.active })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Assign {
    /// `null` takes the order off its rider.
    rider_id: Option<Uuid>,
}

async fn assign(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(number): Path<String>,
    Json(req): Json<Assign>,
) -> Result<Json<Value>> {
    if !admin.role.can_manage_orders() {
        return Err(AppError::Forbidden);
    }
    let mut tx = s.db.begin().await?;
    let (order_id, status): (Uuid, String) =
        sqlx::query_as("SELECT id, status FROM orders WHERE number = $1 FOR UPDATE")
            .bind(&number)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
    if !matches!(
        status.as_str(),
        "confirmed" | "processing" | "packed" | "shipped" | "out-for-delivery"
    ) {
        return Err(AppError::invalid(format!(
            "An order that is {status} can't be given to a rider."
        )));
    }
    let rider_name = match req.rider_id {
        Some(rid) => Some(
            sqlx::query_scalar::<_, String>("SELECT name FROM riders WHERE id = $1 AND active")
                .bind(rid)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| AppError::invalid("Choose an active rider."))?,
        ),
        None => None,
    };
    sqlx::query("UPDATE orders SET rider_id = $2, updated_at = now() WHERE id = $1")
        .bind(order_id)
        .bind(req.rider_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    audit::record(
        &s.db,
        "admin",
        Some(admin.id),
        "order.rider",
        Some(&number),
        json!({ "riderId": req.rider_id }),
    )
    .await;
    Ok(Json(
        json!({ "number": number, "riderId": req.rider_id, "riderName": rider_name }),
    ))
}
