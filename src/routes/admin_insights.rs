//! Staff: the dashboard, reports and customers, read straight from orders and the catalogue so the
//! admin shows the same numbers on every device. All read-only; every staff role may look (the admin
//! website's permission table gives each of them at least a view of these screens).
//!
//! An order "counts" as a sale once it is confirmed and until it is cancelled or returned; orders
//! still waiting for payment, failed or cancelled never count. Days are India days.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::AdminAuth;
use crate::error::{AppError, Result};
use crate::services::orders::today_ist;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/dashboard", get(dashboard))
        .route("/reports/{kind}", get(report))
        .route("/customers", get(customers))
        .route("/customers/{mobile}", get(customer))
}

fn rupees(paise: i64) -> f64 {
    paise as f64 / 100.0
}

/* ───────────────────────── Dashboard ───────────────────────── */

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
struct DayTotal {
    date: NaiveDate,
    orders: i64,
    revenue_paise: i64,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
struct TopProduct {
    product_id: String,
    name: String,
    qty: i64,
    value_paise: i64,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
struct LowStock {
    product_id: String,
    variant_id: String,
    name: String,
    size: String,
    stock: i32,
    low_stock_at: i32,
}

async fn dashboard(State(s): State<AppState>, _admin: AdminAuth) -> Result<Json<Value>> {
    let today = today_ist();
    let mut conn = s.db.acquire().await?;

    const TOTALS: &str = "SELECT
            COUNT(*) FILTER (WHERE (created_at AT TIME ZONE 'Asia/Kolkata')::date = $1 AND status IN ('confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery', 'delivered')),
            COALESCE(SUM(total_paise) FILTER (WHERE (created_at AT TIME ZONE 'Asia/Kolkata')::date = $1 AND status IN ('confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery', 'delivered')), 0)::bigint,
            COUNT(*) FILTER (WHERE status IN ('confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery', 'delivered')),
            COALESCE(SUM(total_paise) FILTER (WHERE status IN ('confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery', 'delivered')), 0)::bigint,
            COUNT(*) FILTER (WHERE status IN ('confirmed', 'processing', 'packed')),
            COUNT(*) FILTER (WHERE status IN ('shipped', 'out-for-delivery')),
            COUNT(*) FILTER (WHERE status = 'pending'),
            COUNT(*) FILTER (WHERE status = 'payment-failed' AND created_at > now() - interval '7 days'),
            COUNT(*) FILTER (WHERE status = 'cancelled' AND payment_status = 'paid')
          FROM orders";
    let (
        today_orders,
        today_paise,
        orders,
        revenue_paise,
        to_ship,
        on_the_way,
        awaiting_payment,
        failed_7d,
        refund_due,
    ): (i64, i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(TOTALS)
        .bind(today)
        .fetch_one(&mut *conn)
        .await?;

    let by_day: Vec<DayTotal> = sqlx::query_as(
        "SELECT d::date AS date,
                COUNT(o.id) AS orders,
                COALESCE(SUM(o.total_paise), 0)::bigint AS revenue_paise
           FROM generate_series($1::date - 13, $1::date, interval '1 day') d
           LEFT JOIN orders o
             ON (o.created_at AT TIME ZONE 'Asia/Kolkata')::date = d::date
            AND o.status IN ('confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery', 'delivered')
          GROUP BY d ORDER BY d",
    )
    .bind(today)
    .fetch_all(&mut *conn)
    .await?;

    let top: Vec<TopProduct> = sqlx::query_as(
        "SELECT i.product_id, MAX(i.name) AS name, SUM(i.qty)::bigint AS qty,
                SUM(i.line_total_paise - i.discount_paise)::bigint AS value_paise
           FROM order_items i JOIN orders o ON o.id = i.order_id
          WHERE o.status IN ('confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery', 'delivered')
            AND o.created_at > now() - interval '30 days'
          GROUP BY i.product_id ORDER BY value_paise DESC LIMIT 5",
    )
    .fetch_all(&mut *conn)
    .await?;

    let low: Vec<LowStock> = sqlx::query_as(
        "SELECT p.id AS product_id, v.id AS variant_id, p.name, v.size, v.stock, v.low_stock_at
           FROM variants v JOIN products p ON p.id = v.product_id
          WHERE p.status = 'active' AND v.stock <= v.low_stock_at
          ORDER BY v.stock, p.name LIMIT 50",
    )
    .fetch_all(&mut *conn)
    .await?;

    let reviews_pending: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM reviews WHERE status = 'pending'")
            .fetch_one(&mut *conn)
            .await?;
    let campaign: Option<(i32, i32, bool)> = sqlx::query_as(
        "SELECT claims, max_claims, active FROM sample_campaigns ORDER BY id LIMIT 1",
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(Json(json!({
        "today": today,
        "todayOrders": today_orders,
        "todayRevenuePaise": today_paise,
        "orders": orders,
        "revenuePaise": revenue_paise,
        "toShip": to_ship,
        "onTheWay": on_the_way,
        "awaitingPayment": awaiting_payment,
        "failedPayments7d": failed_7d,
        "refundDue": refund_due,
        "reviewsPending": reviews_pending,
        "byDay": by_day,
        "topProducts": top,
        "lowStock": low,
        "samples": campaign.map(|(claims, max, active)| json!({ "claims": claims, "maxClaims": max, "active": active })),
    })))
}

/* ───────────────────────── Reports ───────────────────────── */

#[derive(Deserialize)]
struct Range {
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
}

/// One report as a plain table: column names and rows (money in rupees, two decimals).
#[derive(Serialize)]
struct Table {
    head: Vec<&'static str>,
    rows: Vec<Vec<Value>>,
}

async fn report(
    State(s): State<AppState>,
    _admin: AdminAuth,
    Path(kind): Path<String>,
    Query(r): Query<Range>,
) -> Result<Json<Table>> {
    if let (Some(f), Some(t)) = (r.from, r.to)
        && f > t
    {
        return Err(AppError::invalid("“From” is after “To”"));
    }
    let (from, to) = (r.from, r.to);
    let db = &s.db;
    // Orders that count, inside the chosen India days.
    macro_rules! in_range {
        () => {
            "(created_at AT TIME ZONE 'Asia/Kolkata')::date BETWEEN COALESCE($1, '2000-01-01'::date) AND COALESCE($2, '2999-12-31'::date)"
        };
    }
    let table = match kind.as_str() {
        "sales-day" => {
            let rows: Vec<(NaiveDate, i64, i64, i64)> = sqlx::query_as(concat!(
                "SELECT (created_at AT TIME ZONE 'Asia/Kolkata')::date AS d, COUNT(*),
                        COALESCE(SUM((SELECT SUM(qty) FROM order_items i WHERE i.order_id = orders.id)), 0)::bigint,
                        SUM(total_paise)::bigint
                   FROM orders
                  WHERE status IN ('confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery', 'delivered') AND ",
                in_range!(),
                " GROUP BY d ORDER BY d"
            ))
            .bind(from)
            .bind(to)
            .fetch_all(db)
            .await?;
            Table {
                head: vec!["Date", "Orders", "Items", "Revenue ₹"],
                rows: rows
                    .into_iter()
                    .map(|(d, n, q, v)| vec![json!(d), json!(n), json!(q), json!(rupees(v))])
                    .collect(),
            }
        }
        "sales-product" | "sales-category" => {
            let by_category = kind == "sales-category";
            let rows: Vec<(String, i64, i64)> = if by_category {
                sqlx::query_as(
                    "SELECT COALESCE(c.label, p.category_slug, 'Removed products'), SUM(i.qty)::bigint,
                            SUM(i.line_total_paise - i.discount_paise)::bigint AS v
                       FROM order_items i JOIN orders o ON o.id = i.order_id
                       LEFT JOIN products p ON p.id = i.product_id
                       LEFT JOIN categories c ON c.slug = p.category_slug
                      WHERE o.status IN ('confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery', 'delivered')
                        AND (o.created_at AT TIME ZONE 'Asia/Kolkata')::date BETWEEN COALESCE($1, '2000-01-01'::date) AND COALESCE($2, '2999-12-31'::date)
                      GROUP BY 1 ORDER BY v DESC",
)
                .bind(from)
                .bind(to)
                .fetch_all(db)
                .await?
            } else {
                sqlx::query_as(
                    "SELECT i.name || ' · ' || i.size, SUM(i.qty)::bigint,
                            SUM(i.line_total_paise - i.discount_paise)::bigint AS v
                       FROM order_items i JOIN orders o ON o.id = i.order_id
                      WHERE o.status IN ('confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery', 'delivered')
                        AND (o.created_at AT TIME ZONE 'Asia/Kolkata')::date BETWEEN COALESCE($1, '2000-01-01'::date) AND COALESCE($2, '2999-12-31'::date)
                      GROUP BY 1 ORDER BY v DESC",
                )
                .bind(from)
                .bind(to)
                .fetch_all(db)
                .await?
            };
            Table {
                head: vec![
                    if by_category { "Category" } else { "Product" },
                    "Units",
                    "Sales after discounts ₹",
                ],
                rows: rows
                    .into_iter()
                    .map(|(k, q, v)| vec![json!(k), json!(q), json!(rupees(v))])
                    .collect(),
            }
        }
        "sales-payment" => {
            let rows: Vec<(String, i64, i64)> = sqlx::query_as(concat!(
                "SELECT payment_method, COUNT(*), SUM(total_paise)::bigint FROM orders
                  WHERE status IN ('confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery', 'delivered') AND ",
                in_range!(),
                " GROUP BY 1 ORDER BY 3 DESC"
            ))
            .bind(from)
            .bind(to)
            .fetch_all(db)
            .await?;
            Table {
                head: vec!["Payment method", "Orders", "Revenue ₹"],
                rows: rows
                    .into_iter()
                    .map(|(m, n, v)| vec![json!(payment_label(&m)), json!(n), json!(rupees(v))])
                    .collect(),
            }
        }
        "gst" => {
            // Goods only, from each line's own figures fixed at order time. Delivery and COD fees, and
            // returns, are in Finance → GST summary, which reads the ledger.
            let rows: Vec<(String, i32, i64, i64)> = sqlx::query_as(
                "SELECT i.hsn, i.gst_rate_pct, SUM(i.line_total_paise - i.discount_paise)::bigint, SUM(i.gst_paise)::bigint
                   FROM order_items i JOIN orders o ON o.id = i.order_id
                  WHERE o.status IN ('confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery', 'delivered')
                    AND (o.created_at AT TIME ZONE 'Asia/Kolkata')::date BETWEEN COALESCE($1, '2000-01-01'::date) AND COALESCE($2, '2999-12-31'::date)
                  GROUP BY 1, 2 ORDER BY 1, 2",
            )
            .bind(from)
            .bind(to)
            .fetch_all(db)
            .await?;
            Table {
                head: vec!["HSN @ rate", "Taxable ₹", "CGST ₹", "SGST ₹", "Gross ₹"],
                rows: rows
                    .into_iter()
                    .map(|(hsn, rate, gross, gst)| {
                        let cgst = gst / 2;
                        vec![
                            json!(format!(
                                "{} @ {rate}%",
                                if hsn.is_empty() { "—" } else { &hsn }
                            )),
                            json!(rupees(gross - gst)),
                            json!(rupees(cgst)),
                            json!(rupees(gst - cgst)),
                            json!(rupees(gross)),
                        ]
                    })
                    .collect(),
            }
        }
        "inventory" => {
            let rows: Vec<(String, String, String, i32, i32, i64, i64)> = sqlx::query_as(
                "SELECT p.name, v.size, v.sku, v.stock, v.low_stock_at, v.price::bigint * 100,
                        COALESCE((SELECT SUM(i.qty) FROM order_items i JOIN orders o ON o.id = i.order_id
                                   WHERE i.variant_id = v.id
                                     AND o.status IN ('confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery', 'delivered')), 0)::bigint
                   FROM variants v JOIN products p ON p.id = v.product_id
                  ORDER BY p.name, v.size",
            )
            .fetch_all(db)
            .await?;
            Table {
                head: vec![
                    "Product",
                    "Variant",
                    "SKU",
                    "Stock",
                    "Low at",
                    "Price ₹",
                    "Value at price ₹",
                    "Sold (all time)",
                ],
                rows: rows
                    .into_iter()
                    .map(|(name, size, sku, stock, low, price, sold)| {
                        vec![
                            json!(name),
                            json!(size),
                            json!(sku),
                            json!(stock),
                            json!(low),
                            json!(rupees(price)),
                            json!(rupees(price * i64::from(stock.max(0)))),
                            json!(sold),
                        ]
                    })
                    .collect(),
            }
        }
        "coupons" => {
            let rows: Vec<(String, String, i32, i64, i64)> = sqlx::query_as(
                "SELECT c.code, c.kind, c.used,
                        COUNT(o.id),
                        COALESCE(SUM(o.coupon_discount_paise), 0)::bigint
                   FROM coupons c
                   LEFT JOIN orders o ON o.coupon_code = c.code
                    AND o.status IN ('confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery', 'delivered')
                    AND (o.created_at AT TIME ZONE 'Asia/Kolkata')::date BETWEEN COALESCE($1, '2000-01-01'::date) AND COALESCE($2, '2999-12-31'::date)
                  GROUP BY c.code, c.kind, c.used ORDER BY 4 DESC, c.code",
)
            .bind(from)
            .bind(to)
            .fetch_all(db)
            .await?;
            Table {
                head: vec![
                    "Code",
                    "Type",
                    "Uses (all time)",
                    "Orders in range",
                    "Discount given ₹",
                ],
                rows: rows
                    .into_iter()
                    .map(|(code, kind, used, n, v)| {
                        vec![
                            json!(code),
                            json!(kind),
                            json!(used),
                            json!(n),
                            json!(rupees(v)),
                        ]
                    })
                    .collect(),
            }
        }
        "customers" => {
            // New = their first counted order is inside the range; repeat = they had one before it.
            let rows: Vec<(String, Option<String>, i64, i64, bool)> = sqlx::query_as(
                "WITH counted AS (
                    SELECT customer_id, created_at, total_paise FROM orders
                     WHERE status IN ('confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery', 'delivered'))
                 SELECT c.mobile, c.name, COUNT(*), SUM(x.total_paise)::bigint,
                        EXISTS (SELECT 1 FROM counted e WHERE e.customer_id = c.id
                                  AND (e.created_at AT TIME ZONE 'Asia/Kolkata')::date < COALESCE($1, '2000-01-01'::date))
                   FROM counted x JOIN customers c ON c.id = x.customer_id
                  WHERE (x.created_at AT TIME ZONE 'Asia/Kolkata')::date BETWEEN COALESCE($1, '2000-01-01'::date) AND COALESCE($2, '2999-12-31'::date)
                  GROUP BY c.id, c.mobile, c.name ORDER BY 4 DESC",
            )
            .bind(from)
            .bind(to)
            .fetch_all(db)
            .await?;
            Table {
                head: vec!["Mobile", "Name", "Orders", "Spent ₹", "Type"],
                rows: rows
                    .into_iter()
                    .map(|(mobile, name, n, v, before)| {
                        vec![
                            json!(mobile),
                            json!(name.unwrap_or_default()),
                            json!(n),
                            json!(rupees(v)),
                            json!(if before || n > 1 { "Repeat" } else { "New" }),
                        ]
                    })
                    .collect(),
            }
        }
        _ => return Err(AppError::NotFound),
    };
    Ok(Json(table))
}

fn payment_label(m: &str) -> &'static str {
    match m {
        "upi" => "UPI",
        "card" => "Card",
        "netbanking" => "Net banking",
        "wallet" => "Wallet",
        "cod" => "Cash on delivery",
        _ => "Other",
    }
}

/* ───────────────────────── Customers ───────────────────────── */

#[derive(Deserialize)]
struct CustomerQuery {
    /// Name, mobile or email.
    q: Option<String>,
    limit: Option<i64>,
    /// Keyset paging: customers who joined before this time.
    before: Option<DateTime<Utc>>,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
struct CustomerRow {
    mobile: String,
    name: Option<String>,
    email: Option<String>,
    created_at: DateTime<Utc>,
    orders: i64,
    spent_paise: i64,
    last_order_at: Option<DateTime<Utc>>,
}

async fn customers(
    State(s): State<AppState>,
    admin: AdminAuth,
    Query(q): Query<CustomerQuery>,
) -> Result<Json<Vec<CustomerRow>>> {
    if !admin.role.can_view_orders() {
        return Err(AppError::Forbidden);
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let search =
        q.q.as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| {
                format!(
                    "%{}%",
                    t.chars()
                        .take(80)
                        .collect::<String>()
                        .replace(['%', '_', '\\'], "")
                )
            });
    let rows: Vec<CustomerRow> = sqlx::query_as(
        "SELECT c.mobile,
                COALESCE(c.name, (SELECT NULLIF(o.address->>'name', '') FROM orders o WHERE o.customer_id = c.id ORDER BY o.created_at DESC LIMIT 1)) AS name,
                c.email, c.created_at,
                (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id AND o.status NOT IN ('pending', 'payment-failed')) AS orders,
                COALESCE((SELECT SUM(total_paise) FROM orders o WHERE o.customer_id = c.id
                           AND o.status IN ('confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery', 'delivered')), 0)::bigint AS spent_paise,
                (SELECT MAX(created_at) FROM orders o WHERE o.customer_id = c.id AND o.status NOT IN ('pending', 'payment-failed')) AS last_order_at
           FROM customers c
          WHERE ($1::text IS NULL OR c.mobile ILIKE $1 OR c.name ILIKE $1 OR c.email ILIKE $1)
            AND ($2::timestamptz IS NULL OR c.created_at < $2)
          ORDER BY c.created_at DESC
          LIMIT $3",
    )
    .bind(search)
    .bind(q.before)
    .bind(limit)
    .fetch_all(&s.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
struct CustomerOrder {
    id: String,
    created_at: DateTime<Utc>,
    status: String,
    payment_status: String,
    payment: String,
    total: i64,
    coupon_code: Option<String>,
}

async fn customer(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(mobile): Path<String>,
) -> Result<Json<Value>> {
    if !admin.role.can_view_orders() {
        return Err(AppError::Forbidden);
    }
    let valid = mobile.len() == 10
        && mobile.bytes().all(|b| b.is_ascii_digit())
        && matches!(mobile.as_bytes()[0], b'6'..=b'9');
    if !valid {
        return Err(AppError::NotFound);
    }
    let row: CustomerRow = sqlx::query_as(
        "SELECT c.mobile,
                COALESCE(c.name, (SELECT NULLIF(o.address->>'name', '') FROM orders o WHERE o.customer_id = c.id ORDER BY o.created_at DESC LIMIT 1)) AS name,
                c.email, c.created_at,
                (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id AND o.status NOT IN ('pending', 'payment-failed')) AS orders,
                COALESCE((SELECT SUM(total_paise) FROM orders o WHERE o.customer_id = c.id
                           AND o.status IN ('confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery', 'delivered')), 0)::bigint AS spent_paise,
                (SELECT MAX(created_at) FROM orders o WHERE o.customer_id = c.id AND o.status NOT IN ('pending', 'payment-failed')) AS last_order_at
           FROM customers c WHERE c.mobile = $1",
    )
    .bind(&mobile)
    .fetch_optional(&s.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let orders: Vec<CustomerOrder> = sqlx::query_as(
        "SELECT o.number AS id, o.created_at, o.status, o.payment_status, o.payment_method AS payment,
                o.total_paise / 100 AS total, o.coupon_code
           FROM orders o JOIN customers c ON c.id = o.customer_id
          WHERE c.mobile = $1 ORDER BY o.created_at DESC LIMIT 200",
    )
    .bind(&mobile)
    .fetch_all(&s.db)
    .await?;
    Ok(Json(json!({ "customer": row, "orders": orders })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_and_labels() {
        assert_eq!(rupees(12_345), 123.45);
        assert_eq!(payment_label("cod"), "Cash on delivery");
    }
}
