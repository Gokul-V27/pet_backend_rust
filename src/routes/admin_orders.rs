//! Staff: orders (list, detail, move along, cancel, refund) and the books (trial balance, journal,
//! P&L, GST summary). Every change is role-checked and audit-logged; every money move is posted to
//! the ledger by the order service.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::{get, post},
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::AdminAuth;
use crate::error::{AppError, Result};
use crate::repo::audit;
use crate::services::orders::{self, OrderView};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/orders", get(list))
        .route("/orders/{number}", get(one))
        .route("/orders/{number}/status", post(move_status))
        .route("/orders/{number}/refund", post(refund))
        .route("/orders/{number}/shipping", post(shipping))
        .route("/finance/trial-balance", get(trial_balance))
        .route("/finance/journal", get(journal))
        .route("/finance/pnl", get(pnl))
        .route("/finance/gst", get(gst))
}

/* ───────────────────────── Orders ───────────────────────── */

#[derive(Deserialize)]
struct ListQuery {
    status: Option<String>,
    /// Order number or customer mobile.
    q: Option<String>,
    limit: Option<i64>,
    /// Keyset paging: orders created before this time.
    before: Option<DateTime<Utc>>,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
struct OrderSummary {
    id: String,
    created_at: DateTime<Utc>,
    status: String,
    payment_status: String,
    payment: String,
    total: i64,
    item_count: i64,
    customer_name: Option<String>,
    customer_mobile: String,
    pincode: Option<String>,
    slot_label: String,
}

async fn list(
    State(s): State<AppState>,
    admin: AdminAuth,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<OrderSummary>>> {
    if !admin.role.can_view_orders() {
        return Err(AppError::Forbidden);
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let search =
        q.q.as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| format!("%{}%", t.replace(['%', '_'], "")));
    let rows: Vec<OrderSummary> = sqlx::query_as(
        "SELECT o.number AS id, o.created_at, o.status, o.payment_status, o.payment_method AS payment,
                o.total_paise / 100 AS total,
                (SELECT COALESCE(SUM(qty), 0)::bigint FROM order_items i WHERE i.order_id = o.id) AS item_count,
                COALESCE(c.name, NULLIF(o.address->>'name', '')) AS customer_name, c.mobile AS customer_mobile, o.address->>'pincode' AS pincode, o.slot_label
           FROM orders o JOIN customers c ON c.id = o.customer_id
          WHERE ($1::text IS NULL OR o.status = $1)
            AND ($2::text IS NULL OR o.number ILIKE $2 OR c.mobile ILIKE $2)
            AND ($3::timestamptz IS NULL OR o.created_at < $3)
          ORDER BY o.created_at DESC
          LIMIT $4",
    )
    .bind(q.status.as_deref().filter(|st| !st.is_empty()))
    .bind(search)
    .bind(q.before)
    .bind(limit)
    .fetch_all(&s.db)
    .await?;
    Ok(Json(rows))
}

async fn one(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(number): Path<String>,
) -> Result<Json<OrderView>> {
    if !admin.role.can_view_orders() {
        return Err(AppError::Forbidden);
    }
    let mut conn = s.db.acquire().await?;
    Ok(Json(orders::view(&mut conn, &number, None, true).await?))
}

#[derive(Deserialize)]
struct MoveStatus {
    status: String,
    note: Option<String>,
}

async fn move_status(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(number): Path<String>,
    Json(req): Json<MoveStatus>,
) -> Result<Json<OrderView>> {
    if !admin.role.can_manage_orders() {
        return Err(AppError::Forbidden);
    }
    let note = req
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| n.chars().take(300).collect::<String>());
    if req.status == "cancelled" {
        orders::cancel(
            &s,
            &number,
            None,
            "admin",
            Some(admin.id),
            note.as_deref().unwrap_or("Cancelled by Wagwell"),
        )
        .await?;
    } else {
        let mut tx = s.db.begin().await?;
        let (order_id, from): (Uuid, String) =
            sqlx::query_as("SELECT id, status FROM orders WHERE number = $1 FOR UPDATE")
                .bind(&number)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(AppError::NotFound)?;
        if !orders::staff_can_move(&from, &req.status) {
            return Err(AppError::invalid(format!(
                "An order that is {from} can't be moved to {}.",
                req.status
            )));
        }
        sqlx::query("UPDATE orders SET status = $2, updated_at = now() WHERE id = $1")
            .bind(order_id)
            .bind(&req.status)
            .execute(&mut *tx)
            .await?;
        orders::add_event(
            &mut tx,
            order_id,
            &req.status,
            note.as_deref(),
            "admin",
            Some(admin.id),
        )
        .await?;
        match req.status.as_str() {
            "delivered" => orders::on_delivered(&mut tx, order_id, &number).await?,
            "returned" => orders::release_stock(&mut tx, order_id, &number, "Returned").await?,
            _ => {}
        }
        tx.commit().await?;
    }
    audit::record(
        &s.db,
        "admin",
        Some(admin.id),
        "order.status",
        Some(&number),
        json!({ "to": req.status }),
    )
    .await;
    let mut conn = s.db.acquire().await?;
    Ok(Json(orders::view(&mut conn, &number, None, true).await?))
}

#[derive(Deserialize)]
struct Shipping {
    courier: String,
    awb: String,
}

/// Courier and tracking number, shown to the customer on their order page.
async fn shipping(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(number): Path<String>,
    Json(req): Json<Shipping>,
) -> Result<Json<OrderView>> {
    if !admin.role.can_manage_orders() {
        return Err(AppError::Forbidden);
    }
    let (courier, awb) = (req.courier.trim(), req.awb.trim());
    if courier.is_empty() || courier.chars().count() > 40 {
        return Err(AppError::invalid("Choose a courier."));
    }
    if awb.is_empty()
        || awb.len() > 40
        || !awb.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(AppError::invalid(
            "Enter the tracking number (letters, numbers and dashes).",
        ));
    }
    let updated = sqlx::query(
        "UPDATE orders SET courier = $2, awb = $3, updated_at = now() WHERE number = $1",
    )
    .bind(&number)
    .bind(courier)
    .bind(awb)
    .execute(&s.db)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(AppError::NotFound);
    }
    audit::record(
        &s.db,
        "admin",
        Some(admin.id),
        "order.shipping",
        Some(&number),
        json!({ "courier": courier, "awb": awb }),
    )
    .await;
    let mut conn = s.db.acquire().await?;
    Ok(Json(orders::view(&mut conn, &number, None, true).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefundRequest {
    /// Rupees; omit to refund everything still refundable.
    amount: Option<i64>,
    reason: String,
}

async fn refund(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(number): Path<String>,
    Json(req): Json<RefundRequest>,
) -> Result<Json<OrderView>> {
    if !admin.role.can_handle_money() {
        return Err(AppError::Forbidden);
    }
    let reason = req.reason.trim();
    if reason.is_empty() || reason.chars().count() > 300 {
        return Err(AppError::invalid("Give a short reason for the refund."));
    }
    // Rupees to paise, refusing absurd input instead of overflowing.
    let amount = match req.amount {
        Some(r) => Some(
            r.checked_mul(100)
                .filter(|p| *p > 0)
                .ok_or_else(|| AppError::invalid("Enter the refund in whole rupees."))?,
        ),
        None => None,
    };
    let mut conn = s.db.acquire().await?;
    let (order_id,): (Uuid,) = sqlx::query_as("SELECT id FROM orders WHERE number = $1")
        .bind(&number)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(AppError::NotFound)?;
    let (payment_id,): (Uuid,) = sqlx::query_as(
        "SELECT id FROM payments WHERE order_id = $1 AND status IN ('captured', 'partially_refunded') ORDER BY created_at LIMIT 1",
    )
    .bind(order_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| AppError::invalid("Nothing has been paid on this order, or it is already fully refunded."))?;
    drop(conn);
    // The service checks the order is cancelled or delivered, reserves the amount, asks the
    // gateway, and books it; it runs in its own task so closing the page can't stop it half way.
    orders::refund_detached(&s, payment_id, amount, reason.to_owned(), Some(admin.id)).await?;
    audit::record(
        &s.db,
        "admin",
        Some(admin.id),
        "order.refund",
        Some(&number),
        json!({ "paise": amount, "reason": reason }),
    )
    .await;
    let mut conn = s.db.acquire().await?;
    Ok(Json(orders::view(&mut conn, &number, None, true).await?))
}

/* ───────────────────────── Finance ───────────────────────── */

#[derive(Deserialize)]
struct Range {
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
}

impl Range {
    /// Inclusive dates in India time; defaults to this month so far.
    fn bounds(&self) -> Result<(NaiveDate, NaiveDate)> {
        let today = orders::today_ist();
        let from = self
            .from
            .unwrap_or_else(|| today.with_day0(0).unwrap_or(today));
        let to = self.to.unwrap_or(today);
        if from > to || (to - from).num_days() > 400 {
            return Err(AppError::invalid(
                "Pick a date range of up to about a year.",
            ));
        }
        Ok((from, to))
    }
}

use chrono::Datelike;

fn money_admin(admin: &AdminAuth) -> Result<()> {
    if admin.role.can_handle_money() {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
struct AccountTotal {
    code: String,
    name: String,
    kind: String,
    debit_paise: i64,
    credit_paise: i64,
}

/// Every account's debits and credits up to a date. Debits always equal credits overall.
async fn trial_balance(
    State(s): State<AppState>,
    admin: AdminAuth,
    Query(r): Query<Range>,
) -> Result<Json<Value>> {
    money_admin(&admin)?;
    // An as-of date needs no range: any past date works on its own.
    let to = r.to.unwrap_or_else(orders::today_ist);
    let rows: Vec<AccountTotal> = sqlx::query_as(
        "SELECT a.code, a.name, a.kind,
                COALESCE(SUM(l.debit_paise) FILTER (WHERE (e.posted_at AT TIME ZONE 'Asia/Kolkata')::date <= $1), 0)::bigint AS debit_paise,
                COALESCE(SUM(l.credit_paise) FILTER (WHERE (e.posted_at AT TIME ZONE 'Asia/Kolkata')::date <= $1), 0)::bigint AS credit_paise
           FROM ledger_accounts a
           LEFT JOIN journal_lines l ON l.account_code = a.code
           LEFT JOIN journal_entries e ON e.id = l.entry_id
          GROUP BY a.code, a.name, a.kind ORDER BY a.code",
    )
    .bind(to)
    .fetch_all(&s.db)
    .await?;
    let debits: i64 = rows.iter().map(|r| r.debit_paise).sum();
    let credits: i64 = rows.iter().map(|r| r.credit_paise).sum();
    Ok(Json(
        json!({ "asOf": to, "accounts": rows, "totalDebitPaise": debits, "totalCreditPaise": credits, "balanced": debits == credits }),
    ))
}

#[derive(Deserialize)]
struct JournalQuery {
    limit: Option<i64>,
    before: Option<DateTime<Utc>>,
}

/// The journal, newest first, each entry with its lines (read-only).
async fn journal(
    State(s): State<AppState>,
    admin: AdminAuth,
    Query(q): Query<JournalQuery>,
) -> Result<Json<Value>> {
    money_admin(&admin)?;
    let entries: Vec<(Uuid, DateTime<Utc>, String, String, String, String)> = sqlx::query_as(
        "SELECT id, posted_at, memo, source_type, purpose, created_by FROM journal_entries
          WHERE ($1::timestamptz IS NULL OR posted_at < $1) ORDER BY posted_at DESC LIMIT $2",
    )
    .bind(q.before)
    .bind(q.limit.unwrap_or(50).clamp(1, 200))
    .fetch_all(&s.db)
    .await?;
    let ids: Vec<Uuid> = entries.iter().map(|e| e.0).collect();
    let lines: Vec<(Uuid, String, String, i64, i64)> = sqlx::query_as(
        "SELECT l.entry_id, l.account_code, a.name, l.debit_paise, l.credit_paise
           FROM journal_lines l JOIN ledger_accounts a ON a.code = l.account_code
          WHERE l.entry_id = ANY($1) ORDER BY l.id",
    )
    .bind(&ids)
    .fetch_all(&s.db)
    .await?;
    let out: Vec<Value> = entries
        .into_iter()
        .map(|(id, at, memo, source, purpose, by)| {
            let ls: Vec<Value> = lines
                .iter()
                .filter(|l| l.0 == id)
                .map(|l| json!({ "account": l.1, "name": l.2, "debitPaise": l.3, "creditPaise": l.4 }))
                .collect();
            json!({ "id": id, "postedAt": at, "memo": memo, "source": source, "purpose": purpose, "createdBy": by, "lines": ls })
        })
        .collect();
    Ok(Json(json!(out)))
}

/// Profit and loss for a date range, straight from the ledger. (Cost of goods arrives with
/// vendors and purchase prices; until then this is gross income less returns.)
async fn pnl(
    State(s): State<AppState>,
    admin: AdminAuth,
    Query(r): Query<Range>,
) -> Result<Json<Value>> {
    money_admin(&admin)?;
    let (from, to) = r.bounds()?;
    let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT a.code, a.name, a.kind,
                (CASE WHEN a.kind = 'income' THEN SUM(l.credit_paise - l.debit_paise) ELSE SUM(l.debit_paise - l.credit_paise) END)::bigint
           FROM journal_lines l
           JOIN journal_entries e ON e.id = l.entry_id
           JOIN ledger_accounts a ON a.code = l.account_code
          WHERE a.kind IN ('income', 'expense')
            AND (e.posted_at AT TIME ZONE 'Asia/Kolkata')::date BETWEEN $1 AND $2
          GROUP BY a.code, a.name, a.kind ORDER BY a.code",
    )
    .bind(from)
    .bind(to)
    .fetch_all(&s.db)
    .await?;
    let income: i64 = rows.iter().filter(|r| r.2 == "income").map(|r| r.3).sum();
    let expense: i64 = rows.iter().filter(|r| r.2 == "expense").map(|r| r.3).sum();
    let lines: Vec<Value> = rows
        .iter()
        .map(|r| json!({ "code": r.0, "name": r.1, "kind": r.2, "amountPaise": r.3 }))
        .collect();
    Ok(Json(
        json!({ "from": from, "to": to, "lines": lines, "incomePaise": income, "expensePaise": expense, "netPaise": income - expense }),
    ))
}

/// GST on delivered orders, by rate (from the per-line figures fixed at order time), plus fees.
async fn gst(
    State(s): State<AppState>,
    admin: AdminAuth,
    Query(r): Query<Range>,
) -> Result<Json<Value>> {
    money_admin(&admin)?;
    let (from, to) = r.bounds()?;
    let by_rate: Vec<(i32, i64, i64)> = sqlx::query_as(
        "SELECT i.gst_rate_pct,
                SUM(i.line_total_paise - i.discount_paise - i.gst_paise)::bigint AS taxable,
                SUM(i.gst_paise)::bigint AS gst
           FROM order_items i JOIN orders o ON o.id = i.order_id
           JOIN journal_entries e ON e.source_type = 'order' AND e.source_id = o.id AND e.purpose = 'delivered'
          WHERE (e.posted_at AT TIME ZONE 'Asia/Kolkata')::date BETWEEN $1 AND $2
          GROUP BY i.gst_rate_pct ORDER BY i.gst_rate_pct",
    )
    .bind(from)
    .bind(to)
    .fetch_all(&s.db)
    .await?;
    let (gst_total,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(l.credit_paise - l.debit_paise), 0)::bigint
           FROM journal_lines l JOIN journal_entries e ON e.id = l.entry_id
          WHERE l.account_code = '2100' AND (e.posted_at AT TIME ZONE 'Asia/Kolkata')::date BETWEEN $1 AND $2",
    )
    .bind(from)
    .bind(to)
    .fetch_one(&s.db)
    .await?;
    let rates: Vec<Value> = by_rate
        .iter()
        .map(|r| json!({ "ratePct": r.0, "taxablePaise": r.1, "gstPaise": r.2 }))
        .collect();
    Ok(Json(json!({
        "from": from,
        "to": to,
        "goodsByRate": rates,
        // All GST booked in the period: goods + delivery/COD fees, less GST on returns.
        "netGstPayablePaise": gst_total,
        "note": "Intra-state (Tamil Nadu) supplies: split each figure equally into CGST and SGST. Confirm HSN codes and rates with your CA."
    })))
}
