//! Orders from cart to delivery: price on the server, reserve stock, take payment, move through the
//! status flow, refund — and post every money event to the ledger in the same transaction.
//! HTTP-free so both the shop routes and the admin routes share exactly one set of rules.

use chrono::{DateTime, Days, FixedOffset, NaiveDate, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Connection, FromRow, PgConnection};
use uuid::Uuid;

use crate::error::{AppError, Result};
use crate::services::ledger::{self, OrderAmounts};
use crate::services::pricing::{
    self, CouponRule, DeliveryKind, LineInput, PricingInput, Quote, paise,
};
use crate::state::AppState;

/// "Feed a Stray" add-on, rupees (same as the admin's default donation setting).
pub const DONATION_RUPEES: i64 = 5;
pub const MAX_LINES: usize = 50;
pub const MAX_QTY: i64 = 20;

/// How long an online order may wait for payment before it's closed and its stock released.
pub const UNPAID_MINUTES: i64 = 30;

/// Same-day delivery is offered for orders placed before this hour (India time).
pub const EXPRESS_CUTOFF_HOUR: u32 = 12;
const EXPRESS_WINDOW: &str = "6–9 pm";
const STANDARD_WINDOWS: [&str; 2] = ["9 am – 1 pm", "4 – 8 pm"];

fn ist() -> FixedOffset {
    FixedOffset::east_opt(5 * 3600 + 30 * 60).expect("IST offset is valid")
}

/// Now, in India.
pub fn now_ist() -> DateTime<FixedOffset> {
    Utc::now().with_timezone(&ist())
}

/// Today in India.
pub fn today_ist() -> NaiveDate {
    now_ist().date_naive()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CartLine {
    pub product_id: String,
    pub variant_id: String,
    pub qty: i64,
    #[serde(default)]
    pub autoship: bool,
    pub frequency_days: Option<i32>,
}

/// Same Chennai rules as the website's checkPincode.
pub struct Serviceability {
    pub express: bool,
    /// Days from today to the standard delivery date.
    pub standard_days: u64,
}

pub fn serviceable(pincode: &str) -> Option<Serviceability> {
    let n: u32 = pincode.parse().ok().filter(|_| pincode.len() == 6)?;
    match n {
        600_001..=600_130 => Some(Serviceability {
            express: true,
            standard_days: 1,
        }),
        601_101..=603_404 | 631_501..=631_605 => Some(Serviceability {
            express: false,
            standard_days: 2,
        }),
        _ => None,
    }
}

/// "Today", "Tomorrow", else "Thu 8 Oct".
fn day_label(date: NaiveDate, today: NaiveDate) -> String {
    if date == today {
        "Today".into()
    } else if today.checked_add_days(Days::new(1)) == Some(date) {
        "Tomorrow".into()
    } else {
        date.format("%a %-d %b").to_string()
    }
}

/// Checks a delivery slot against what the shop actually offers right now and returns the label to
/// store. Same-day ("express") only before the cut-off and only today; standard only on the first
/// standard day for the pincode. The stored label is built here, never taken from the browser.
pub fn checked_slot_label(
    pincode: &str,
    kind: &str,
    date: NaiveDate,
    label: &str,
    now: DateTime<FixedOffset>,
) -> Result<String> {
    let sv = serviceable(pincode).ok_or_else(|| {
        AppError::invalid("We deliver in and around Chennai for now. Check the pincode.")
    })?;
    let today = now.date_naive();
    let window = label.rsplit_once(", ").map_or("", |(_, w)| w.trim());
    let gone =
        || AppError::invalid("That delivery time is no longer available. Please choose another.");
    match kind {
        "express" => {
            if !sv.express {
                return Err(AppError::invalid(
                    "Same-day delivery isn't available for that pincode.",
                ));
            }
            if date != today || now.hour() >= EXPRESS_CUTOFF_HOUR || window != EXPRESS_WINDOW {
                return Err(gone());
            }
            Ok(format!("Today, {EXPRESS_WINDOW}"))
        }
        "standard" => {
            let earliest = today.checked_add_days(Days::new(sv.standard_days));
            if Some(date) != earliest || !STANDARD_WINDOWS.contains(&window) {
                return Err(gone());
            }
            Ok(format!("{}, {window}", day_label(date, today)))
        }
        _ => Err(AppError::invalid("Choose a delivery time.")),
    }
}

#[derive(FromRow)]
struct CatalogueRow {
    product_id: String,
    name: String,
    category_slug: String,
    gst_rate_pct: i32,
    hsn: String,
    status: String,
    autoship_eligible: bool,
    variant_id: String,
    size: String,
    price: i32,
    mrp: i32,
}

/// Merges repeated lines and checks quantities.
pub fn tidy_lines(lines: &[CartLine]) -> Result<Vec<CartLine>> {
    if lines.is_empty() {
        return Err(AppError::invalid("Your cart is empty."));
    }
    if lines.len() > MAX_LINES {
        return Err(AppError::invalid(
            "That's too many different items for one order.",
        ));
    }
    let mut out: Vec<CartLine> = Vec::new();
    for l in lines {
        if l.qty < 1 || l.qty > MAX_QTY {
            return Err(AppError::invalid(format!(
                "Choose between 1 and {MAX_QTY} of each item."
            )));
        }
        if let Some(f) = l.frequency_days.filter(|_| l.autoship)
            && !(7..=120).contains(&f)
        {
            return Err(AppError::invalid(
                "Choose an Autoship frequency between 7 and 120 days.",
            ));
        }
        match out
            .iter_mut()
            .find(|o| o.variant_id == l.variant_id && o.autoship == l.autoship)
        {
            Some(o) => o.qty = (o.qty + l.qty).min(MAX_QTY),
            None => out.push(l.clone()),
        }
    }
    // A fixed order for row locks, so two checkouts can never wait on each other.
    out.sort_by(|a, b| a.variant_id.cmp(&b.variant_id));
    Ok(out)
}

/// Catalogue facts for each line, read from the database. With `lock`, the variant rows are
/// locked until the transaction ends, so stock can't be sold twice.
pub async fn load_lines(
    conn: &mut PgConnection,
    lines: &[CartLine],
    lock: bool,
) -> Result<Vec<LineInput>> {
    const READ: &str =
        "SELECT p.id AS product_id, p.name, p.category_slug, p.gst_rate_pct, p.hsn, p.status, p.autoship_eligible,
                v.id AS variant_id, v.size, v.price, v.mrp
           FROM variants v JOIN products p ON p.id = v.product_id
          WHERE v.id = $1 AND p.id = $2";
    const READ_AND_LOCK: &str =
        "SELECT p.id AS product_id, p.name, p.category_slug, p.gst_rate_pct, p.hsn, p.status, p.autoship_eligible,
                v.id AS variant_id, v.size, v.price, v.mrp
           FROM variants v JOIN products p ON p.id = v.product_id
          WHERE v.id = $1 AND p.id = $2
            FOR UPDATE OF v";
    let sql = if lock { READ_AND_LOCK } else { READ };
    let mut out = Vec::with_capacity(lines.len());
    for l in lines {
        let row: Option<CatalogueRow> = sqlx::query_as(sql)
            .bind(&l.variant_id)
            .bind(&l.product_id)
            .fetch_optional(&mut *conn)
            .await?;
        let row = row.filter(|r| r.status == "active").ok_or_else(|| {
            AppError::invalid(
                "One of the items in your cart is no longer sold. Remove it and try again.",
            )
        })?;
        if l.autoship && !row.autoship_eligible {
            return Err(AppError::invalid(format!(
                "{} can’t be put on Autoship.",
                row.name
            )));
        }
        out.push(LineInput {
            product_id: row.product_id,
            variant_id: row.variant_id,
            category: row.category_slug,
            name: row.name,
            size: row.size,
            hsn: row.hsn,
            gst_rate_pct: i64::from(row.gst_rate_pct),
            price: i64::from(row.price),
            mrp: i64::from(row.mrp),
            qty: l.qty,
            autoship: l.autoship,
            frequency_days: l.frequency_days.filter(|_| l.autoship),
        });
    }
    Ok(out)
}

/// With `lock`, the coupon row is held until the transaction ends, so its usage limit can't be
/// passed by orders placed at the same moment.
pub async fn load_coupon(
    conn: &mut PgConnection,
    code: &str,
    lock: bool,
) -> Result<Option<CouponRule>> {
    #[derive(FromRow)]
    struct Row {
        code: String,
        kind: String,
        value: i32,
        max_discount: Option<i32>,
        min_order: i32,
        first_order_only: bool,
        categories: Vec<String>,
        product_ids: Vec<String>,
        excludes_autoship: bool,
        starts_at: Option<String>,
        ends_at: Option<String>,
        usage_limit: Option<i32>,
        per_customer_limit: Option<i32>,
        used: i32,
        active: bool,
    }
    const READ: &str = "SELECT code, kind, value, max_discount, min_order, first_order_only, categories, product_ids,
                excludes_autoship, starts_at, ends_at, usage_limit, per_customer_limit, used, active
           FROM coupons WHERE code = upper(trim($1))";
    const READ_AND_LOCK: &str = "SELECT code, kind, value, max_discount, min_order, first_order_only, categories, product_ids,
                excludes_autoship, starts_at, ends_at, usage_limit, per_customer_limit, used, active
           FROM coupons WHERE code = upper(trim($1)) FOR UPDATE";
    let row: Option<Row> = sqlx::query_as(if lock { READ_AND_LOCK } else { READ })
        .bind(code)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(row.map(|r| CouponRule {
        code: r.code,
        kind: r.kind,
        value: i64::from(r.value),
        max_discount: r.max_discount.map(i64::from),
        min_order: i64::from(r.min_order),
        first_order_only: r.first_order_only,
        categories: r.categories,
        product_ids: r.product_ids,
        excludes_autoship: r.excludes_autoship,
        starts_at: r.starts_at,
        ends_at: r.ends_at,
        usage_limit: r.usage_limit.map(i64::from),
        per_customer_limit: r.per_customer_limit.map(i64::from),
        used: i64::from(r.used),
        active: r.active,
    }))
}

/// What the browser asks for. Prices are never taken from it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteRequest {
    pub lines: Vec<CartLine>,
    pub coupon_code: Option<String>,
    #[serde(default)]
    pub delivery: Option<String>,
    #[serde(default)]
    pub payment: Option<String>,
    #[serde(default)]
    pub donate: bool,
}

/// Prices a cart for this customer, inside the caller's transaction.
pub async fn quote(
    conn: &mut PgConnection,
    customer_id: Option<Uuid>,
    req: &QuoteRequest,
    lock: bool,
) -> Result<(Vec<CartLine>, Quote)> {
    let lines = tidy_lines(&req.lines)?;
    let inputs = load_lines(&mut *conn, &lines, lock).await?;
    let code = req
        .coupon_code
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    let coupon = match code {
        Some(c) => load_coupon(&mut *conn, c, lock).await?,
        None => None,
    };
    let (is_first_order, coupon_uses) = match customer_id {
        Some(id) => {
            let (placed,): (i64,) = sqlx::query_as(
                "SELECT count(*) FROM orders WHERE customer_id = $1 AND status <> 'payment-failed'",
            )
            .bind(id)
            .fetch_one(&mut *conn)
            .await?;
            let uses = match &coupon {
                Some(c) => {
                    let (n,): (i64,) = sqlx::query_as(
                        "SELECT count(*) FROM orders WHERE customer_id = $1 AND coupon_code = $2 AND status <> 'payment-failed'",
                    )
                    .bind(id)
                    .bind(&c.code)
                    .fetch_one(&mut *conn)
                    .await?;
                    n
                }
                None => 0,
            };
            (placed == 0, uses)
        }
        None => (true, 0),
    };
    let q = pricing::compute(&PricingInput {
        lines: inputs,
        coupon_code: code,
        coupon,
        is_first_order,
        coupon_uses_by_customer: coupon_uses,
        delivery: if req.delivery.as_deref() == Some("express") {
            DeliveryKind::Express
        } else {
            DeliveryKind::Standard
        },
        cash_on_delivery: req.payment.as_deref() == Some("cod"),
        donation: if req.donate { DONATION_RUPEES } else { 0 },
        today: today_ist().format("%Y-%m-%d").to_string(),
    });
    Ok((lines, q))
}

/// One stock-taking line: what to take and how to name it if it has run out.
#[derive(Debug, Clone)]
pub struct StockItem {
    pub product_id: String,
    pub variant_id: String,
    pub name: String,
    pub size: String,
    pub qty: i64,
}

/// Takes stock for the priced lines or fails with the item that ran out (the caller's transaction
/// rolls back).
pub async fn take_stock(conn: &mut PgConnection, q: &Quote, number: &str) -> Result<()> {
    let items: Vec<StockItem> = q
        .lines
        .iter()
        .map(|l| StockItem {
            product_id: l.product_id.clone(),
            variant_id: l.variant_id.clone(),
            name: l.name.clone(),
            size: l.size.clone(),
            qty: l.qty,
        })
        .collect();
    take_stock_items(conn, &items, number).await
}

pub async fn take_stock_items(
    conn: &mut PgConnection,
    items: &[StockItem],
    number: &str,
) -> Result<()> {
    for l in items {
        let after: Option<(i32,)> = sqlx::query_as(
            "UPDATE variants SET stock = stock - $1, updated_at = now() WHERE id = $2 AND stock >= $1 RETURNING stock",
        )
        .bind(l.qty as i32)
        .bind(&l.variant_id)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((after,)) = after else {
            return Err(AppError::OutOfStock(format!(
                "{} ({}) has just sold out or doesn't have {} left. Lower the quantity or remove it.",
                l.name, l.size, l.qty
            )));
        };
        sqlx::query(
            "INSERT INTO inventory_txns (variant_id, product_id, type, qty, before_qty, after_qty, reason, by_whom)
             VALUES ($1, $2, 'sale', $3, $4, $5, $6, 'System')",
        )
        .bind(&l.variant_id)
        .bind(&l.product_id)
        .bind(-(l.qty as i32))
        .bind(after + l.qty as i32)
        .bind(after)
        .bind(format!("Order {number}"))
        .execute(&mut *conn)
        .await?;
    }
    // Units sold, for the shop's "popular" ordering; given back if the order is cancelled.
    add_popularity(
        conn,
        items.iter().map(|l| (l.product_id.as_str(), l.qty as i32)),
    )
    .await
}

/// Changes products' sold-units counters, one update per product in product-id order, so two
/// orders touching the same products always lock them in the same order (no deadlock).
async fn add_popularity<'a>(
    conn: &mut PgConnection,
    lines: impl Iterator<Item = (&'a str, i32)>,
) -> Result<()> {
    let mut per_product = std::collections::BTreeMap::<&str, i32>::new();
    for (product, qty) in lines {
        *per_product.entry(product).or_default() += qty;
    }
    for (product, qty) in per_product {
        sqlx::query("UPDATE products SET popularity = GREATEST(popularity + $2, 0) WHERE id = $1")
            .bind(product)
            .bind(qty)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// An order's lines in lock order (by variant), as stock items.
async fn order_stock_items(conn: &mut PgConnection, order_id: Uuid) -> Result<Vec<StockItem>> {
    let rows: Vec<(String, String, String, String, i32)> = sqlx::query_as(
        "SELECT product_id, variant_id, name, size, qty FROM order_items WHERE order_id = $1 ORDER BY variant_id",
    )
    .bind(order_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(product_id, variant_id, name, size, qty)| StockItem {
            product_id,
            variant_id,
            name,
            size,
            qty: i64::from(qty),
        })
        .collect())
}

/// Puts an order's stock back (cancelled, unpaid or returned).
pub async fn release_stock(
    conn: &mut PgConnection,
    order_id: Uuid,
    number: &str,
    why: &str,
) -> Result<()> {
    let items = order_stock_items(conn, order_id).await?;
    for it in &items {
        let after: Option<(i32,)> = sqlx::query_as(
            "UPDATE variants SET stock = stock + $1, updated_at = now() WHERE id = $2 RETURNING stock",
        )
        .bind(it.qty as i32)
        .bind(&it.variant_id)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some((after,)) = after {
            sqlx::query(
                "INSERT INTO inventory_txns (variant_id, product_id, type, qty, before_qty, after_qty, reason, by_whom)
                 VALUES ($1, $2, 'return', $3, $4, $5, $6, 'System')",
            )
            .bind(&it.variant_id)
            .bind(&it.product_id)
            .bind(it.qty as i32)
            .bind(after - it.qty as i32)
            .bind(after)
            .bind(format!("{why} · {number}"))
            .execute(&mut *conn)
            .await?;
        }
    }
    add_popularity(
        conn,
        items
            .iter()
            .map(|it| (it.product_id.as_str(), -(it.qty as i32))),
    )
    .await
}

pub async fn add_event(
    conn: &mut PgConnection,
    order_id: Uuid,
    status: &str,
    note: Option<&str>,
    actor_type: &str,
    actor_id: Option<Uuid>,
) -> Result<()> {
    sqlx::query("INSERT INTO order_events (order_id, status, note, actor_type, actor_id) VALUES ($1, $2, $3, $4, $5)")
        .bind(order_id)
        .bind(status)
        .bind(note)
        .bind(actor_type)
        .bind(actor_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The order's money facts for the ledger.
pub async fn amounts(conn: &mut PgConnection, order_id: Uuid) -> Result<OrderAmounts> {
    let (total, delivery, cod_fee, donation, gst, method): (i64, i64, i64, i64, i64, String) = sqlx::query_as(
        "SELECT total_paise, delivery_paise, cod_fee_paise, donation_paise, gst_paise, payment_method FROM orders WHERE id = $1",
    )
    .bind(order_id)
    .fetch_one(&mut *conn)
    .await?;
    let lines: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT line_total_paise - discount_paise, gst_paise FROM order_items WHERE order_id = $1 ORDER BY id",
    )
    .bind(order_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(OrderAmounts {
        total,
        lines,
        delivery,
        cod_fee,
        donation,
        gst,
        paid_online: method != "cod",
    })
}

#[derive(Debug, Clone, FromRow)]
pub struct PaymentRow {
    pub id: Uuid,
    pub order_id: Uuid,
    pub amount_paise: i64,
    pub refunded_paise: i64,
    pub status: String,
    pub gateway_payment_id: Option<String>,
}

/// Locks an order and then its payment (always in this order, so two requests can never wait on
/// each other) and returns the payment with the order's number and status.
async fn lock_payment(
    conn: &mut PgConnection,
    payment_id: Uuid,
) -> Result<(PaymentRow, String, String)> {
    let (order_id,): (Uuid,) = sqlx::query_as("SELECT order_id FROM payments WHERE id = $1")
        .bind(payment_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(AppError::NotFound)?;
    let (number, status): (String, String) =
        sqlx::query_as("SELECT number, status FROM orders WHERE id = $1 FOR UPDATE")
            .bind(order_id)
            .fetch_one(&mut *conn)
            .await?;
    let p: PaymentRow = sqlx::query_as(
        "SELECT id, order_id, amount_paise, refunded_paise, status, gateway_payment_id
           FROM payments WHERE id = $1 FOR UPDATE",
    )
    .bind(payment_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok((p, number, status))
}

/// What happened to a payment the gateway told us about.
#[derive(Debug)]
pub enum Captured {
    /// The order is confirmed (or already was).
    Done,
    /// Money arrived for an order that can't be fulfilled any more (cancelled, or its items sold out
    /// while it waited): refund this payment in full once the transaction has committed.
    RefundDue(Uuid),
    /// Unknown payment, or the amount isn't what was asked for.
    Rejected,
}

/// Marks a gateway payment captured: confirms the order and posts the money to customer advances.
///
/// Safe to call any number of times, from the browser's confirmation and from the webhook. It never
/// fails because of stock: a payment is always recorded, and when the order can no longer be
/// fulfilled the caller is told to refund it (`RefundDue`).
pub async fn capture(
    conn: &mut PgConnection,
    gateway_order_id: &str,
    gateway_payment_id: &str,
    amount_paise: Option<i64>,
) -> Result<Captured> {
    let found: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM payments WHERE gateway_order_id = $1")
            .bind(gateway_order_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((payment_id,)) = found else {
        return Ok(Captured::Rejected);
    };
    let (pay, number, status) = lock_payment(&mut *conn, payment_id).await?;
    if amount_paise.is_some_and(|a| a != pay.amount_paise) {
        tracing::error!(payment = %pay.id, "captured amount differs from the order; left for review");
        return Ok(Captured::Rejected);
    }
    if pay.status != "pending" && pay.status != "failed" {
        return Ok(Captured::Done); // already handled
    }
    sqlx::query("UPDATE payments SET status = 'captured', gateway_payment_id = $2, updated_at = now() WHERE id = $1")
        .bind(pay.id)
        .bind(gateway_payment_id)
        .execute(&mut *conn)
        .await?;
    ledger::post(
        &mut *conn,
        "payment",
        pay.id,
        "captured",
        &format!("Payment received for {number}"),
        "gateway",
        &ledger::payment_captured(pay.amount_paise)?,
    )
    .await?;

    match status.as_str() {
        "pending" => {
            confirm_order(&mut *conn, pay.order_id, "Payment received").await?;
            Ok(Captured::Done)
        }
        "payment-failed" => {
            // The order was closed (unpaid for too long) and its stock released. Try to take the
            // stock again inside a savepoint, so running out can never undo the payment record.
            let items = order_stock_items(&mut *conn, pay.order_id).await?;
            let mut sp = Connection::begin(&mut *conn).await?;
            match take_stock_items(&mut sp, &items, &number).await {
                Ok(()) => {
                    sp.commit().await?;
                    sqlx::query("UPDATE coupons SET used = used + 1, updated_at = now() WHERE code = (SELECT coupon_code FROM orders WHERE id = $1)")
                        .bind(pay.order_id)
                        .execute(&mut *conn)
                        .await?;
                    confirm_order(
                        &mut *conn,
                        pay.order_id,
                        "Payment received (after the checkout had timed out)",
                    )
                    .await?;
                    Ok(Captured::Done)
                }
                Err(AppError::OutOfStock(_)) => {
                    sp.rollback().await?;
                    sqlx::query("UPDATE orders SET status = 'cancelled', payment_status = 'paid', updated_at = now() WHERE id = $1")
                        .bind(pay.order_id)
                        .execute(&mut *conn)
                        .await?;
                    add_event(&mut *conn, pay.order_id, "cancelled", Some("Paid after the checkout timed out and the items had sold out: refunded in full"), "gateway", None).await?;
                    Ok(Captured::RefundDue(pay.id))
                }
                Err(e) => Err(e),
            }
        }
        _ => {
            // Cancelled (or otherwise closed) while the customer was paying.
            sqlx::query("UPDATE orders SET payment_status = 'paid', updated_at = now() WHERE id = $1 AND payment_status <> 'paid'")
                .bind(pay.order_id)
                .execute(&mut *conn)
                .await?;
            add_event(
                &mut *conn,
                pay.order_id,
                &status,
                Some("Payment arrived after the order was cancelled: refunded in full"),
                "gateway",
                None,
            )
            .await?;
            Ok(Captured::RefundDue(pay.id))
        }
    }
}

async fn confirm_order(conn: &mut PgConnection, order_id: Uuid, note: &str) -> Result<()> {
    sqlx::query("UPDATE orders SET status = 'confirmed', payment_status = 'paid', updated_at = now() WHERE id = $1")
        .bind(order_id)
        .execute(&mut *conn)
        .await?;
    add_event(
        &mut *conn,
        order_id,
        "confirmed",
        Some(note),
        "gateway",
        None,
    )
    .await
}

/// Closes an order that was never paid: its stock goes back, its coupon use is given back, and any
/// open payment is marked failed. `final_status` is 'payment-failed' (timed out or replaced) or
/// 'cancelled' (the customer or staff cancelled it before paying). Does nothing, and returns false,
/// unless the order is still 'pending' once locked, so a payment arriving at the same moment wins.
pub async fn close_unpaid(
    conn: &mut PgConnection,
    order_id: Uuid,
    final_status: &str,
    note: &str,
    actor_type: &str,
    actor_id: Option<Uuid>,
) -> Result<bool> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT status, number FROM orders WHERE id = $1 FOR UPDATE")
            .bind(order_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((status, number)) = row else {
        return Ok(false);
    };
    if status != "pending" {
        return Ok(false);
    }
    sqlx::query("UPDATE payments SET status = 'failed', updated_at = now() WHERE order_id = $1 AND status = 'pending'")
        .bind(order_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE orders SET status = $2, payment_status = 'failed', updated_at = now() WHERE id = $1")
        .bind(order_id)
        .bind(final_status)
        .execute(&mut *conn)
        .await?;
    release_stock(&mut *conn, order_id, &number, "Not paid").await?;
    sqlx::query("UPDATE coupons SET used = GREATEST(used - 1, 0), updated_at = now() WHERE code = (SELECT coupon_code FROM orders WHERE id = $1)")
        .bind(order_id)
        .execute(&mut *conn)
        .await?;
    add_event(
        &mut *conn,
        order_id,
        final_status,
        Some(note),
        actor_type,
        actor_id,
    )
    .await?;
    Ok(true)
}

/// A customer's other unpaid online checkouts are closed when they start a new one, so an
/// abandoned attempt can't hold stock or count as their first order / coupon use.
pub async fn close_customer_pending(conn: &mut PgConnection, customer_id: Uuid) -> Result<()> {
    let ids: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM orders WHERE customer_id = $1 AND status = 'pending' AND payment_method <> 'cod' ORDER BY created_at",
    )
    .bind(customer_id)
    .fetch_all(&mut *conn)
    .await?;
    for (id,) in ids {
        close_unpaid(
            &mut *conn,
            id,
            "payment-failed",
            "Replaced by a newer checkout",
            "system",
            None,
        )
        .await?;
    }
    Ok(())
}

/// Closes online orders that were never paid, including ones whose payment never got as far as
/// being opened with the gateway. Each is handled in its own transaction and re-checked under lock.
/// Returns how many were closed.
pub async fn expire_unpaid(db: &sqlx::PgPool) -> Result<u64> {
    let stale: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM orders
          WHERE status = 'pending' AND payment_method <> 'cod'
            AND created_at < now() - ($1::int * interval '1 minute')
          ORDER BY created_at LIMIT 100",
    )
    .bind(UNPAID_MINUTES as i32)
    .fetch_all(db)
    .await?;
    let mut closed = 0;
    for (id,) in stale {
        let mut tx = db.begin().await?;
        if close_unpaid(
            &mut tx,
            id,
            "payment-failed",
            "Payment not completed in time",
            "system",
            None,
        )
        .await?
        {
            closed += 1;
        }
        tx.commit().await?;
    }
    Ok(closed)
}

/// Opens the Razorpay order for an online order whose payment row already exists. If the gateway
/// can't be reached the order is closed (stock released) so nothing is left half-done.
pub async fn attach_gateway_order(
    s: &AppState,
    order_id: Uuid,
    number: &str,
    amount_paise: i64,
) -> Result<()> {
    let rzp = s.razorpay.as_ref().ok_or(AppError::PaymentsUnavailable)?;
    match rzp.create_order(amount_paise, number).await {
        Ok(gateway_order_id) => {
            sqlx::query(
                "UPDATE payments SET gateway_order_id = $2, updated_at = now()
                  WHERE order_id = $1 AND status = 'pending' AND gateway_order_id IS NULL",
            )
            .bind(order_id)
            .bind(gateway_order_id)
            .execute(&s.db)
            .await?;
            Ok(())
        }
        Err(e) => {
            tracing::error!(error = ?e, order = number, "could not open a Razorpay order");
            let mut tx = s.db.begin().await?;
            close_unpaid(
                &mut tx,
                order_id,
                "payment-failed",
                "Payment couldn't start",
                "system",
                None,
            )
            .await?;
            tx.commit().await?;
            Err(AppError::PaymentsUnavailable)
        }
    }
}

/// Which status moves staff may make. Payment-driven moves (pending → confirmed / payment-failed)
/// only ever come from the payment flow.
pub fn staff_can_move(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("pending", "cancelled")
            | ("confirmed", "processing" | "packed" | "cancelled")
            | ("processing", "packed" | "cancelled")
            | ("packed", "shipped" | "cancelled")
            | ("shipped", "out-for-delivery" | "delivered" | "cancelled")
            | ("out-for-delivery", "delivered" | "cancelled")
            | ("delivered", "returned")
    )
}

/// Customers may cancel until the order is packed.
pub fn customer_can_cancel(status: &str) -> bool {
    matches!(status, "pending" | "confirmed" | "processing")
}

/// Delivered: cash-on-delivery money is now collected, and revenue, GST and the donation are booked.
pub async fn on_delivered(conn: &mut PgConnection, order_id: Uuid, number: &str) -> Result<()> {
    let a = amounts(&mut *conn, order_id).await?;
    if !a.paid_online && a.total > 0 {
        sqlx::query(
            "INSERT INTO payments (order_id, provider, method, amount_paise, status) VALUES ($1, 'cod', 'cod', $2, 'captured')",
        )
        .bind(order_id)
        .bind(a.total)
        .execute(&mut *conn)
        .await?;
        sqlx::query("UPDATE orders SET payment_status = 'paid' WHERE id = $1")
            .bind(order_id)
            .execute(&mut *conn)
            .await?;
    }
    if a.total > 0 {
        ledger::post(
            &mut *conn,
            "order",
            order_id,
            "delivered",
            &format!("{number} delivered"),
            "system",
            &ledger::delivered(&a)?,
        )
        .await?;
    }
    Ok(())
}

/// The captured payment for an order, if any (online or cash collected on delivery).
pub async fn captured_payment(
    conn: &mut PgConnection,
    order_id: Uuid,
) -> Result<Option<PaymentRow>> {
    Ok(sqlx::query_as(
        "SELECT id, order_id, amount_paise, refunded_paise, status, gateway_payment_id
           FROM payments WHERE order_id = $1 AND status IN ('captured', 'partially_refunded')
          ORDER BY created_at LIMIT 1 FOR UPDATE",
    )
    .bind(order_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Cancels an order (the customer before packing, or staff before delivery): stock goes back and a
/// paid order is refunded in full. `customer_id` limits it to that customer's own order.
///
/// The cancel is committed first and stands even if the refund then fails: the refund error is
/// logged and noted on the order ("refund it from the order page"), never returned as a failure.
pub async fn cancel(
    s: &AppState,
    number: &str,
    customer_id: Option<Uuid>,
    actor_type: &str,
    actor_id: Option<Uuid>,
    note: &str,
) -> Result<()> {
    let mut tx = s.db.begin().await?;
    let (order_id, status): (Uuid, String) = sqlx::query_as(
        "SELECT id, status FROM orders WHERE number = $1 AND ($2::uuid IS NULL OR customer_id = $2) FOR UPDATE",
    )
    .bind(number)
    .bind(customer_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let allowed = if actor_type == "customer" {
        customer_can_cancel(&status)
    } else {
        staff_can_move(&status, "cancelled")
    };
    if !allowed {
        return Err(AppError::invalid(
            "This order can't be cancelled any more. Contact us and we'll help.",
        ));
    }
    let refund_payment_id = if status == "pending" {
        // Never paid: close it like an expired checkout, with the cancel as its final status.
        close_unpaid(&mut tx, order_id, "cancelled", note, actor_type, actor_id).await?;
        None
    } else {
        sqlx::query("UPDATE orders SET status = 'cancelled', updated_at = now() WHERE id = $1")
            .bind(order_id)
            .execute(&mut *tx)
            .await?;
        release_stock(&mut tx, order_id, number, "Cancelled").await?;
        add_event(
            &mut tx,
            order_id,
            "cancelled",
            Some(note),
            actor_type,
            actor_id,
        )
        .await?;
        let paid = captured_payment(&mut tx, order_id).await?;
        if paid.is_none() {
            sqlx::query("UPDATE orders SET payment_status = 'refunded' WHERE id = $1 AND payment_status = 'cod-due'")
                .bind(order_id)
                .execute(&mut *tx)
                .await?;
        }
        paid.map(|p| p.id)
    };
    tx.commit().await?;
    if let Some(payment_id) = refund_payment_id
        && let Err(e) = refund_detached(s, payment_id, None, note.to_owned(), actor_id).await
    {
        tracing::error!(error = ?e, order = number, "refund after cancel failed");
        let mut conn = s.db.acquire().await?;
        add_event(
            &mut conn,
            order_id,
            "cancelled",
            Some("The automatic refund failed: refund it from the order page"),
            "system",
            None,
        )
        .await?;
    }
    Ok(())
}

/* ───────────────────────── Refunds ───────────────────────── */

/// Refunds a payment in a task of its own, so a customer or staff member closing the page can't
/// stop it half way (money sent but not booked). `amount` in paise; `None` refunds what is left.
pub async fn refund_detached(
    s: &AppState,
    payment_id: Uuid,
    amount: Option<i64>,
    reason: String,
    by: Option<Uuid>,
) -> Result<()> {
    let s = s.clone();
    tokio::spawn(async move { refund_payment(&s, payment_id, amount, &reason, by).await })
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("refund task failed: {e}")))?
}

/// Sends money back and books it.
///
/// 1. In one transaction the payment is locked and the amount is *reserved* as a pending refund
///    (counting every other pending refund), so two refunds at once can never pay out more than was
///    paid.
/// 2. The gateway is asked to refund (online payments); cash payments skip this.
/// 3. In a second transaction the refund is booked: payment totals, order status, ledger.
///
/// A refund that was sent but not booked stays 'pending' and is booked when Razorpay's
/// refund.processed webhook arrives.
pub async fn refund_payment(
    s: &AppState,
    payment_id: Uuid,
    amount: Option<i64>,
    reason: &str,
    by: Option<Uuid>,
) -> Result<()> {
    let mut tx = s.db.begin().await?;
    let (p, _number, order_status) = lock_payment(&mut tx, payment_id).await?;
    if !matches!(p.status.as_str(), "captured" | "partially_refunded") {
        return Err(AppError::invalid(
            "Nothing has been paid on this order, or it is already fully refunded.",
        ));
    }
    if !matches!(
        order_status.as_str(),
        "cancelled" | "delivered" | "returned"
    ) {
        return Err(AppError::invalid(
            "Cancel the order to refund it before delivery, or refund after it is delivered.",
        ));
    }
    let (pending,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(amount_paise), 0)::bigint FROM refunds WHERE payment_id = $1 AND status = 'pending'",
    )
    .bind(p.id)
    .fetch_one(&mut *tx)
    .await?;
    let left = p.amount_paise - p.refunded_paise - pending;
    let amount = match amount {
        Some(a) if a > 0 && a <= left => a,
        Some(_) => {
            return Err(AppError::invalid(format!(
                "Refund between ₹1 and ₹{}.",
                left / 100
            )));
        }
        None if left > 0 => left,
        None => return Err(AppError::invalid("Nothing left to refund.")),
    };
    let (refund_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO refunds (payment_id, amount_paise, reason, status, created_by) VALUES ($1, $2, $3, 'pending', $4) RETURNING id",
    )
    .bind(p.id)
    .bind(amount)
    .bind(reason)
    .bind(by)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    if let Some(gateway_payment_id) = &p.gateway_payment_id {
        let sent = match s.razorpay.as_ref() {
            Some(r) => r.refund(gateway_payment_id, amount).await,
            None => Err(anyhow::anyhow!("Razorpay is not configured")),
        };
        match sent {
            Ok(gateway_refund_id) => {
                sqlx::query("UPDATE refunds SET gateway_refund_id = $2 WHERE id = $1")
                    .bind(refund_id)
                    .bind(gateway_refund_id)
                    .execute(&s.db)
                    .await?;
            }
            Err(e) => {
                tracing::error!(error = ?e, payment = %p.id, "razorpay refund failed");
                sqlx::query("UPDATE refunds SET status = 'failed' WHERE id = $1")
                    .bind(refund_id)
                    .execute(&s.db)
                    .await?;
                return Err(AppError::PaymentsUnavailable);
            }
        }
    }

    let mut tx = s.db.begin().await?;
    let by_label = by.map_or_else(|| "system".to_owned(), |id| id.to_string());
    book_refund(&mut tx, refund_id, &by_label).await?;
    tx.commit().await?;
    Ok(())
}

/// Books a refund that the gateway accepted: payment totals, order payment status and the ledger
/// (reversing the advance for a cancelled order, or a sales return for a delivered one). Claims the
/// refund first, so booking it twice (browser flow and webhook) changes nothing the second time.
pub async fn book_refund(conn: &mut PgConnection, refund_id: Uuid, by: &str) -> Result<()> {
    let claimed: Option<(Uuid, i64)> = sqlx::query_as(
        "UPDATE refunds SET status = 'processed' WHERE id = $1 AND status = 'pending' RETURNING payment_id, amount_paise",
    )
    .bind(refund_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((payment_id, amount)) = claimed else {
        return Ok(());
    };
    let (p, number, order_status) = lock_payment(&mut *conn, payment_id).await?;
    let after_delivery = matches!(order_status.as_str(), "delivered" | "returned");
    let refunded = p.refunded_paise + amount;
    let pay_status = if refunded >= p.amount_paise {
        "refunded"
    } else {
        "partially_refunded"
    };
    sqlx::query(
        "UPDATE payments SET refunded_paise = $2, status = $3, updated_at = now() WHERE id = $1",
    )
    .bind(p.id)
    .bind(refunded)
    .bind(pay_status)
    .execute(&mut *conn)
    .await?;
    let order_pay_status = if pay_status == "refunded" {
        "refunded"
    } else {
        "partially-refunded"
    };
    sqlx::query("UPDATE orders SET payment_status = $2, updated_at = now() WHERE id = $1")
        .bind(p.order_id)
        .bind(order_pay_status)
        .execute(&mut *conn)
        .await?;
    let a = amounts(&mut *conn, p.order_id).await?;
    ledger::post(
        &mut *conn,
        "refund",
        refund_id,
        "refund",
        &format!("Refund for {number}"),
        by,
        &ledger::refund(amount, p.refunded_paise, &a, after_delivery)?,
    )
    .await?;
    Ok(())
}

/// Razorpay says a refund it accepted has failed (e.g. a closed bank account). If we had already
/// booked it, the booking is reversed so the books, the payment and the order show that the money
/// is still with us and can be refunded again.
pub async fn refund_failed(conn: &mut PgConnection, gateway_refund_id: &str) -> Result<()> {
    let found: Option<(Uuid, Uuid)> =
        sqlx::query_as("SELECT id, payment_id FROM refunds WHERE gateway_refund_id = $1")
            .bind(gateway_refund_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((refund_id, payment_id)) = found else {
        return Ok(());
    };
    let (p, number, _status) = lock_payment(&mut *conn, payment_id).await?;
    let row: Option<(String, i64)> =
        sqlx::query_as("SELECT status, amount_paise FROM refunds WHERE id = $1 FOR UPDATE")
            .bind(refund_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((status, amount)) = row else {
        return Ok(());
    };
    match status.as_str() {
        "failed" => Ok(()),
        "pending" => {
            sqlx::query("UPDATE refunds SET status = 'failed' WHERE id = $1")
                .bind(refund_id)
                .execute(&mut *conn)
                .await?;
            Ok(())
        }
        _ => {
            sqlx::query("UPDATE refunds SET status = 'failed' WHERE id = $1")
                .bind(refund_id)
                .execute(&mut *conn)
                .await?;
            let refunded = (p.refunded_paise - amount).max(0);
            let (pay_status, order_pay_status) = if refunded == 0 {
                ("captured", "paid")
            } else {
                ("partially_refunded", "partially-refunded")
            };
            sqlx::query("UPDATE payments SET refunded_paise = $2, status = $3, updated_at = now() WHERE id = $1")
                .bind(p.id)
                .bind(refunded)
                .bind(pay_status)
                .execute(&mut *conn)
                .await?;
            sqlx::query("UPDATE orders SET payment_status = $2, updated_at = now() WHERE id = $1")
                .bind(p.order_id)
                .bind(order_pay_status)
                .execute(&mut *conn)
                .await?;
            let original = ledger::lines_of(&mut *conn, "refund", refund_id, "refund").await?;
            if !original.is_empty() {
                ledger::post(
                    &mut *conn,
                    "refund",
                    refund_id,
                    "refund-failed",
                    &format!("Refund for {number} failed at the gateway"),
                    "gateway",
                    &ledger::reversed(&original),
                )
                .await?;
            }
            let (current,): (String,) = sqlx::query_as("SELECT status FROM orders WHERE id = $1")
                .bind(p.order_id)
                .fetch_one(&mut *conn)
                .await?;
            add_event(
                &mut *conn,
                p.order_id,
                &current,
                Some("A refund failed at the gateway: the money is still with us — refund again"),
                "gateway",
                None,
            )
            .await?;
            Ok(())
        }
    }
}

/// refund.processed from Razorpay: books the refund if our own booking never completed.
pub async fn refund_processed(conn: &mut PgConnection, gateway_refund_id: &str) -> Result<()> {
    let found: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM refunds WHERE gateway_refund_id = $1")
            .bind(gateway_refund_id)
            .fetch_optional(&mut *conn)
            .await?;
    if let Some((refund_id,)) = found {
        book_refund(&mut *conn, refund_id, "gateway").await?;
    }
    Ok(())
}

/* ───────────────────────── Views ───────────────────────── */

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderLineView {
    pub product_id: String,
    pub variant_id: String,
    pub name: String,
    pub size: String,
    pub qty: i32,
    /// Rupees.
    pub unit_price: i64,
    pub mrp: i64,
    pub autoship: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_days: Option<i32>,
    pub hsn: String,
    pub gst_rate_pct: i32,
    /// This line's share of the coupon discount, in paise (for invoices).
    pub discount_paise: i64,
    /// GST inside what was paid for this line, in paise (for invoices and GST returns).
    pub gst_paise: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingView {
    pub mrp_total: i64,
    pub items_total: i64,
    pub product_discount: i64,
    pub autoship_savings: i64,
    pub coupon_code: Option<String>,
    pub coupon_discount: i64,
    pub delivery: i64,
    pub cod_fee: i64,
    pub donation: i64,
    pub wallet_used: i64,
    pub total: i64,
    pub gst_included: i64,
    pub item_count: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventView {
    pub status: String,
    pub at: chrono::DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotView {
    pub date: NaiveDate,
    pub label: String,
    pub kind: String,
}

/// An order as both websites show it (rupees, the customer site's field names).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderView {
    pub id: String,
    pub created_at: chrono::DateTime<Utc>,
    pub status: String,
    pub payment_status: String,
    pub payment: String,
    pub address: Value,
    pub slot: SlotView,
    pub lines: Vec<OrderLineView>,
    pub pricing: PricingView,
    pub history: Vec<EventView>,
    pub whatsapp_updates: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub courier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub awb: Option<String>,
    /// Razorpay's payment id once paid online.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment_id: Option<String>,
    /// Only for the admin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub customer: Option<Value>,
    /// Present while an online payment is waiting: what the browser needs to open Razorpay Checkout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub razorpay: Option<Value>,
}

#[derive(FromRow)]
struct OrderRow {
    id: Uuid,
    number: String,
    created_at: chrono::DateTime<Utc>,
    status: String,
    payment_status: String,
    payment_method: String,
    address: Value,
    slot_date: NaiveDate,
    slot_label: String,
    delivery_kind: String,
    mrp_total_paise: i64,
    items_total_paise: i64,
    autoship_savings_paise: i64,
    coupon_code: Option<String>,
    coupon_discount_paise: i64,
    delivery_paise: i64,
    cod_fee_paise: i64,
    donation_paise: i64,
    wallet_used_paise: i64,
    total_paise: i64,
    gst_paise: i64,
    whatsapp_updates: bool,
    courier: Option<String>,
    awb: Option<String>,
}

const fn rupees(p: i64) -> i64 {
    p / 100
}

/// Loads an order by number. `customer_id = Some(..)` limits it to that customer's own orders
/// (anyone else's order looks exactly like a missing one).
pub async fn view(
    conn: &mut PgConnection,
    number: &str,
    customer_id: Option<Uuid>,
    with_customer: bool,
) -> Result<OrderView> {
    let row: OrderRow = sqlx::query_as(
        "SELECT id, number, created_at, status, payment_status, payment_method, address, slot_date, slot_label,
                delivery_kind, mrp_total_paise, items_total_paise, autoship_savings_paise, coupon_code,
                coupon_discount_paise, delivery_paise, cod_fee_paise, donation_paise, wallet_used_paise,
                total_paise, gst_paise, whatsapp_updates, courier, awb
           FROM orders WHERE number = $1 AND ($2::uuid IS NULL OR customer_id = $2)",
    )
    .bind(number)
    .bind(customer_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(AppError::NotFound)?;
    #[derive(FromRow)]
    struct ItemRow {
        product_id: String,
        variant_id: String,
        name: String,
        size: String,
        qty: i32,
        unit_price_paise: i64,
        mrp_paise: i64,
        autoship: bool,
        frequency_days: Option<i32>,
        hsn: String,
        gst_rate_pct: i32,
        discount_paise: i64,
        gst_paise: i64,
    }
    let lines: Vec<ItemRow> = sqlx::query_as(
        "SELECT product_id, variant_id, name, size, qty, unit_price_paise, mrp_paise, autoship, frequency_days,
                hsn, gst_rate_pct, discount_paise, gst_paise
           FROM order_items WHERE order_id = $1 ORDER BY id",
    )
    .bind(row.id)
    .fetch_all(&mut *conn)
    .await?;
    let history: Vec<(String, chrono::DateTime<Utc>, Option<String>)> = sqlx::query_as(
        "SELECT status, at, note FROM order_events WHERE order_id = $1 ORDER BY at, id",
    )
    .bind(row.id)
    .fetch_all(&mut *conn)
    .await?;
    let razorpay = if row.status == "pending" {
        let p: Option<(String, i64)> = sqlx::query_as(
            "SELECT gateway_order_id, amount_paise FROM payments WHERE order_id = $1 AND status = 'pending' AND gateway_order_id IS NOT NULL",
        )
        .bind(row.id)
        .fetch_optional(&mut *conn)
        .await?;
        p.map(|(gateway_order_id, amount_paise)| serde_json::json!({ "gatewayOrderId": gateway_order_id, "amountPaise": amount_paise }))
    } else {
        None
    };
    let customer = if with_customer {
        let c: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT c.mobile, COALESCE(c.name, NULLIF(o.address->>'name', '')) FROM customers c JOIN orders o ON o.customer_id = c.id WHERE o.id = $1",
        )
        .bind(row.id)
        .fetch_optional(&mut *conn)
        .await?;
        c.map(|(mobile, name)| serde_json::json!({ "mobile": mobile, "name": name }))
    } else {
        None
    };
    let item_count = lines.iter().map(|l| i64::from(l.qty)).sum();
    let payment_id: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT gateway_payment_id FROM payments WHERE order_id = $1 AND gateway_payment_id IS NOT NULL ORDER BY created_at LIMIT 1",
    )
    .bind(row.id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(OrderView {
        id: row.number,
        created_at: row.created_at,
        status: row.status,
        payment_status: row.payment_status,
        payment: row.payment_method,
        address: row.address,
        slot: SlotView {
            date: row.slot_date,
            label: row.slot_label,
            kind: row.delivery_kind,
        },
        lines: lines
            .into_iter()
            .map(|l| OrderLineView {
                product_id: l.product_id,
                variant_id: l.variant_id,
                name: l.name,
                size: l.size,
                qty: l.qty,
                unit_price: rupees(l.unit_price_paise),
                mrp: rupees(l.mrp_paise),
                autoship: l.autoship,
                frequency_days: l.frequency_days,
                hsn: l.hsn,
                gst_rate_pct: l.gst_rate_pct,
                discount_paise: l.discount_paise,
                gst_paise: l.gst_paise,
            })
            .collect(),
        pricing: PricingView {
            mrp_total: rupees(row.mrp_total_paise),
            items_total: rupees(row.items_total_paise),
            product_discount: rupees(
                row.mrp_total_paise - row.items_total_paise - row.autoship_savings_paise,
            ),
            autoship_savings: rupees(row.autoship_savings_paise),
            coupon_code: row.coupon_code,
            coupon_discount: rupees(row.coupon_discount_paise),
            delivery: rupees(row.delivery_paise),
            cod_fee: rupees(row.cod_fee_paise),
            donation: rupees(row.donation_paise),
            wallet_used: rupees(row.wallet_used_paise),
            total: rupees(row.total_paise),
            gst_included: (row.gst_paise + 50) / 100,
            item_count,
        },
        history: history
            .into_iter()
            .map(|(status, at, note)| EventView { status, at, note })
            .collect(),
        whatsapp_updates: row.whatsapp_updates,
        courier: row.courier,
        awb: row.awb,
        payment_id: payment_id.and_then(|(p,)| p),
        customer,
        razorpay,
    })
}

/// Saves a priced order (status and payment status already decided by the caller).
#[allow(clippy::too_many_arguments)]
pub async fn insert_order(
    conn: &mut PgConnection,
    customer_id: Uuid,
    idempotency_key: &str,
    q: &Quote,
    payment_method: &str,
    status: &str,
    payment_status: &str,
    address: &Value,
    slot_date: NaiveDate,
    slot_label: &str,
    delivery_kind: &str,
    whatsapp_updates: bool,
    request_hash: &str,
) -> Result<(Uuid, String)> {
    let (id, number): (Uuid, String) = sqlx::query_as(
        "INSERT INTO orders (number, customer_id, idempotency_key, status, payment_method, payment_status, address,
                             slot_date, slot_label, delivery_kind, mrp_total_paise, items_total_paise, autoship_savings_paise,
                             coupon_code, coupon_discount_paise, delivery_paise, cod_fee_paise, donation_paise,
                             wallet_used_paise, total_paise, gst_paise, whatsapp_updates, request_hash)
         VALUES ('WAG-' || nextval('order_number_seq'), $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                 $15, $16, $17, $18, $19, $20, $21, $22)
         RETURNING id, number",
    )
    .bind(customer_id)
    .bind(idempotency_key)
    .bind(status)
    .bind(payment_method)
    .bind(payment_status)
    .bind(address)
    .bind(slot_date)
    .bind(slot_label)
    .bind(delivery_kind)
    .bind(paise(q.mrp_total))
    .bind(paise(q.items_total))
    .bind(paise(q.autoship_savings))
    .bind(&q.coupon_code)
    .bind(paise(q.coupon_discount))
    .bind(paise(q.delivery))
    .bind(paise(q.cod_fee))
    .bind(paise(q.donation))
    .bind(paise(q.wallet_used))
    .bind(paise(q.total))
    .bind(q.gst_paise)
    .bind(whatsapp_updates)
    .bind(request_hash)
    .fetch_one(&mut *conn)
    .await?;
    for l in &q.lines {
        sqlx::query(
            "INSERT INTO order_items (order_id, product_id, variant_id, name, size, qty, unit_price_paise, mrp_paise,
                                      gst_rate_pct, hsn, line_total_paise, discount_paise, gst_paise, autoship, frequency_days)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        )
        .bind(id)
        .bind(&l.product_id)
        .bind(&l.variant_id)
        .bind(&l.name)
        .bind(&l.size)
        .bind(l.qty as i32)
        .bind(paise(l.unit_price))
        .bind(paise(l.mrp))
        .bind(l.gst_rate_pct as i32)
        .bind(&l.hsn)
        .bind(paise(l.line_total))
        .bind(l.discount_paise)
        .bind(l.gst_paise)
        .bind(l.autoship)
        .bind(l.frequency_days)
        .execute(&mut *conn)
        .await?;
    }
    if let Some(code) = &q.coupon_code {
        // The usage limit is enforced in the update itself (the coupon row is also locked by quote).
        let taken = sqlx::query(
            "UPDATE coupons SET used = used + 1, updated_at = now()
              WHERE code = $1 AND (usage_limit IS NULL OR used < usage_limit)",
        )
        .bind(code)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if taken == 0 {
            return Err(AppError::invalid(format!("{code} has been fully used up.")));
        }
    }
    Ok((id, number))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(variant: &str, qty: i64, autoship: bool) -> CartLine {
        CartLine {
            product_id: "p".into(),
            variant_id: variant.into(),
            qty,
            autoship,
            frequency_days: autoship.then_some(30),
        }
    }

    #[test]
    fn repeated_lines_merge_and_quantities_are_checked() {
        let t = tidy_lines(&[
            line("b", 2, false),
            line("a", 1, false),
            line("b", 3, false),
            line("b", 1, true),
        ])
        .unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].variant_id, "a"); // sorted for lock order
        assert_eq!(
            t.iter()
                .find(|l| l.variant_id == "b" && !l.autoship)
                .unwrap()
                .qty,
            5
        );
        assert!(tidy_lines(&[]).is_err());
        assert!(tidy_lines(&[line("a", 0, false)]).is_err());
        assert!(tidy_lines(&[line("a", MAX_QTY + 1, false)]).is_err());
        let mut bad = line("a", 1, true);
        bad.frequency_days = Some(400);
        assert!(tidy_lines(&[bad]).is_err());
    }

    #[test]
    fn chennai_pincodes_match_the_website() {
        assert!(serviceable("600020").is_some_and(|s| s.express));
        assert!(serviceable("603103").is_some_and(|s| !s.express));
        assert!(serviceable("560001").is_none());
        assert!(serviceable("60002").is_none());
        assert!(serviceable("abcdef").is_none());
    }

    fn ist_at(date: &str, hour: u32) -> DateTime<FixedOffset> {
        let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap();
        d.and_hms_opt(hour, 30, 0)
            .unwrap()
            .and_local_timezone(ist())
            .unwrap()
    }

    fn day(date: &str) -> NaiveDate {
        NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn same_day_slots_close_at_noon_and_only_apply_today() {
        let morning = ist_at("2026-10-05", 9);
        let label = checked_slot_label(
            "600020",
            "express",
            day("2026-10-05"),
            "Today, 6–9 pm",
            morning,
        )
        .unwrap();
        assert_eq!(label, "Today, 6–9 pm");
        // After the cut-off, on another day, for a pincode without same-day, or with another window.
        assert!(
            checked_slot_label(
                "600020",
                "express",
                day("2026-10-05"),
                "Today, 6–9 pm",
                ist_at("2026-10-05", 12)
            )
            .is_err()
        );
        assert!(
            checked_slot_label(
                "600020",
                "express",
                day("2026-10-08"),
                "Today, 6–9 pm",
                morning
            )
            .is_err()
        );
        assert!(
            checked_slot_label(
                "603103",
                "express",
                day("2026-10-05"),
                "Today, 6–9 pm",
                morning
            )
            .is_err()
        );
        assert!(
            checked_slot_label(
                "600020",
                "express",
                day("2026-10-05"),
                "Today, 9 am – 1 pm",
                morning
            )
            .is_err()
        );
    }

    #[test]
    fn standard_slots_are_the_first_delivery_day_for_the_pincode() {
        let now = ist_at("2026-10-05", 15);
        // Chennai: tomorrow. The stored label is built here, whatever the browser wrote.
        let label = checked_slot_label(
            "600020",
            "standard",
            day("2026-10-06"),
            "Whenever, 4 – 8 pm",
            now,
        )
        .unwrap();
        assert_eq!(label, "Tomorrow, 4 – 8 pm");
        // Greater Chennai: two days out.
        let label = checked_slot_label(
            "603103",
            "standard",
            day("2026-10-07"),
            "x, 9 am – 1 pm",
            now,
        )
        .unwrap();
        assert_eq!(label, "Wed 7 Oct, 9 am – 1 pm");
        assert!(
            checked_slot_label(
                "603103",
                "standard",
                day("2026-10-06"),
                "x, 9 am – 1 pm",
                now
            )
            .is_err()
        );
        assert!(
            checked_slot_label(
                "600020",
                "standard",
                day("2026-10-05"),
                "x, 9 am – 1 pm",
                now
            )
            .is_err()
        );
        assert!(
            checked_slot_label("600020", "standard", day("2026-10-06"), "x, midnight", now)
                .is_err()
        );
        assert!(
            checked_slot_label("600020", "weekly", day("2026-10-06"), "x, 9 am – 1 pm", now)
                .is_err()
        );
        assert!(
            checked_slot_label(
                "560001",
                "standard",
                day("2026-10-06"),
                "x, 9 am – 1 pm",
                now
            )
            .is_err()
        );
    }

    #[test]
    fn status_moves_follow_the_flow() {
        assert!(staff_can_move("confirmed", "packed"));
        assert!(staff_can_move("shipped", "delivered"));
        assert!(!staff_can_move("pending", "confirmed")); // only a payment confirms
        // A parcel that is refused or lost on the way can be cancelled (stock back, refund).
        assert!(staff_can_move("shipped", "cancelled"));
        assert!(staff_can_move("out-for-delivery", "cancelled"));
        assert!(!staff_can_move("delivered", "cancelled"));
        assert!(!staff_can_move("cancelled", "confirmed"));
        assert!(customer_can_cancel("confirmed") && !customer_can_cancel("shipped"));
    }
}
