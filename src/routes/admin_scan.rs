//! Staff phone app: look a pack up by its barcode, receive stock (goods-in) with its batch and
//! expiry, and see which received batches are close to their expiry date.
//!
//! Receiving is idempotent: the app sends an `Idempotency-Key` with every goods-in, so a double tap
//! or a retry after a lost answer returns the first result instead of adding the stock twice.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::HeaderMap,
    routing::{get, post},
};
use chrono::{Days, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::auth::AdminAuth;
use crate::error::{AppError, Result};
use crate::repo::audit;
use crate::services::orders::today_ist;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/barcodes/{code}", get(lookup))
        .route("/inventory/receipts", post(receive))
        .route("/inventory/expiring", get(expiring))
}

/// Lines in one goods-in (a supplier's whole delivery fits comfortably).
pub const MAX_LINES: usize = 200;
const MAX_QTY: i32 = 100_000;
/// Receipt types: new stock, a product's first stock, or goods a customer sent back.
const RECEIPT_TYPES: [&str; 3] = ["addition", "opening", "return"];

/// A barcode or SKU as a scanner reads it: EAN/UPC digits, Code 128 or a SKU like `WG-CR-3K`.
pub fn valid_code(code: &str) -> bool {
    (1..=64).contains(&code.len())
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/* ───────────────────────── Barcode lookup ───────────────────────── */

#[derive(sqlx::FromRow)]
struct MatchRow {
    product_id: String,
    name: String,
    brand: String,
    status: String,
    images: Vec<String>,
    category: String,
    variant_id: String,
    sku: String,
    barcode: Option<String>,
    size: String,
    stock: i32,
    low_stock_at: i32,
    price: i32,
    mrp: i32,
    last_batch_no: Option<String>,
    last_expiry: Option<NaiveDate>,
}

/// `GET /admin/barcodes/{code}` → `{ code, matches: [...] }`. An exact barcode match comes first,
/// then a SKU match. No match is an empty list (the app then offers to create the product).
async fn lookup(
    State(s): State<AppState>,
    _admin: AdminAuth,
    Path(code): Path<String>,
) -> Result<Json<Value>> {
    let code = code.trim().to_owned();
    if !valid_code(&code) {
        return Err(AppError::invalid(
            "That doesn't look like a barcode. Scan again or type the numbers under the bars.",
        ));
    }
    let rows: Vec<MatchRow> = sqlx::query_as(
        "SELECT p.id AS product_id, p.name, p.brand, p.status, p.images, p.category_slug AS category,
                v.id AS variant_id, v.sku, v.barcode, v.size, v.stock, v.low_stock_at, v.price, v.mrp,
                last.batch_no AS last_batch_no, last.expiry_date AS last_expiry
           FROM variants v
           JOIN products p ON p.id = v.product_id
           LEFT JOIN LATERAL (
                SELECT t.batch_no, t.expiry_date FROM inventory_txns t
                 WHERE t.variant_id = v.id AND (t.batch_no IS NOT NULL OR t.expiry_date IS NOT NULL)
                 ORDER BY t.created_at DESC LIMIT 1) last ON true
          WHERE v.barcode = $1 OR lower(v.sku) = lower($1)
          ORDER BY (v.barcode = $1) DESC NULLS LAST, (p.status = 'active') DESC, p.name
          LIMIT 10",
    )
    .bind(&code)
    .fetch_all(&s.db)
    .await?;
    let matches: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "product": { "id": r.product_id, "name": r.name, "brand": r.brand, "status": r.status,
                             "images": r.images, "category": r.category },
                "variant": { "id": r.variant_id, "sku": r.sku, "barcode": r.barcode, "size": r.size,
                             "stock": r.stock, "lowStockAt": r.low_stock_at, "price": r.price, "mrp": r.mrp },
                "lastBatch": (r.last_batch_no.is_some() || r.last_expiry.is_some())
                    .then(|| json!({ "batchNo": r.last_batch_no, "expiryDate": r.last_expiry })),
            })
        })
        .collect();
    Ok(Json(json!({ "code": code, "matches": matches })))
}

/* ───────────────────────── Goods-in ───────────────────────── */

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    #[serde(rename = "type", default = "addition")]
    pub txn_type: String,
    #[serde(default)]
    pub supplier_ref: String,
    #[serde(default)]
    pub note: String,
    pub lines: Vec<ReceiptLine>,
}

fn addition() -> String {
    "addition".into()
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptLine {
    pub variant_id: String,
    pub qty: i32,
    #[serde(default)]
    pub batch_no: Option<String>,
    #[serde(default)]
    pub mfg_date: Option<NaiveDate>,
    #[serde(default)]
    pub expiry_date: Option<NaiveDate>,
    /// What the shop paid per unit, in rupees.
    #[serde(default)]
    pub unit_cost: Option<i32>,
}

/// Printable characters only, trimmed, at most `max`.
fn clean(raw: &str, max: usize) -> String {
    let s: String = raw.chars().filter(|c| !c.is_control()).take(max).collect();
    s.trim().to_owned()
}

/// Tidies a goods-in and checks it against `today` (the shop's date). Messages are shown to staff.
pub fn check(r: &mut Receipt, today: NaiveDate) -> Result<()> {
    if !RECEIPT_TYPES.contains(&r.txn_type.as_str()) {
        return Err(AppError::invalid("Unknown kind of goods-in."));
    }
    if r.lines.is_empty() {
        return Err(AppError::invalid("Add at least one item."));
    }
    if r.lines.len() > MAX_LINES {
        return Err(AppError::invalid(format!(
            "One goods-in can hold at most {MAX_LINES} items; split it in two."
        )));
    }
    r.supplier_ref = clean(&r.supplier_ref, 100);
    r.note = clean(&r.note, 300);
    for (i, l) in r.lines.iter_mut().enumerate() {
        let n = i + 1;
        if l.variant_id.trim().is_empty() {
            return Err(AppError::invalid(format!("Item {n}: choose a product.")));
        }
        if !(1..=MAX_QTY).contains(&l.qty) {
            return Err(AppError::invalid(format!(
                "Item {n}: quantity must be between 1 and {MAX_QTY}."
            )));
        }
        l.batch_no = l
            .batch_no
            .as_deref()
            .map(|b| clean(b, 60))
            .filter(|b| !b.is_empty());
        if let Some(exp) = l.expiry_date
            && exp < today
        {
            return Err(AppError::invalid(format!(
                "Item {n}: that batch expired on {exp}. Expired stock can't be received."
            )));
        }
        if let Some(mfg) = l.mfg_date {
            if mfg > today {
                return Err(AppError::invalid(format!(
                    "Item {n}: the manufacturing date is in the future."
                )));
            }
            if l.expiry_date.is_some_and(|exp| mfg > exp) {
                return Err(AppError::invalid(format!(
                    "Item {n}: the manufacturing date is after the expiry date."
                )));
            }
        }
        if l.unit_cost.is_some_and(|c| !(0..=10_000_000).contains(&c)) {
            return Err(AppError::invalid(format!(
                "Item {n}: the cost price doesn't look right."
            )));
        }
    }
    Ok(())
}

/// The `Idempotency-Key` header: 8–100 printable ASCII characters.
fn idempotency_key(headers: &HeaderMap) -> Result<String> {
    headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| (8..=100).contains(&k.len()) && k.bytes().all(|b| b.is_ascii_graphic()))
        .map(str::to_owned)
        .ok_or_else(|| AppError::invalid("Missing Idempotency-Key header."))
}

/// `POST /admin/inventory/receipts` with `{ type?, supplierRef?, note?, lines: [...] }`
/// → `{ receiptId, type, lines: [{ variantId, productId, qty, before, after, batchNo, expiryDate }] }`.
async fn receive(
    State(s): State<AppState>,
    admin: AdminAuth,
    headers: HeaderMap,
    Json(mut req): Json<Receipt>,
) -> Result<Json<Value>> {
    if !admin.role.can_adjust_stock() {
        return Err(AppError::Forbidden);
    }
    let key = idempotency_key(&headers)?;
    check(&mut req, today_ist())?;
    let request_hash = hex::encode(Sha256::digest(
        serde_json::to_vec(&req).map_err(anyhow::Error::from)?,
    ));

    let mut tx = s.db.begin().await?;
    // A second request with the same key waits here until the first commits, then finds its row.
    let receipt_id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO inventory_receipts (admin_id, idempotency_key, request_hash, supplier_ref, note)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (admin_id, idempotency_key) DO NOTHING RETURNING id",
    )
    .bind(admin.id)
    .bind(&key)
    .bind(&request_hash)
    .bind(&req.supplier_ref)
    .bind(&req.note)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(receipt_id) = receipt_id else {
        let (hash, mut result): (String, Value) = sqlx::query_as(
            "SELECT request_hash, result FROM inventory_receipts WHERE admin_id = $1 AND idempotency_key = $2",
        )
        .bind(admin.id)
        .bind(&key)
        .fetch_one(&mut *tx)
        .await?;
        if hash != request_hash {
            return Err(AppError::invalid(
                "That goods-in was already saved with different items. Please try again.",
            ));
        }
        result["replayed"] = Value::Bool(true);
        return Ok(Json(result));
    };

    let reason = clean(
        &match (req.supplier_ref.is_empty(), req.note.is_empty()) {
            (true, true) => "Goods in".to_owned(),
            (false, true) => format!("Goods in · {}", req.supplier_ref),
            (true, false) => format!("Goods in · {}", req.note),
            (false, false) => format!("Goods in · {} · {}", req.supplier_ref, req.note),
        },
        200,
    );
    let mut lines = Vec::with_capacity(req.lines.len());
    for (i, l) in req.lines.iter().enumerate() {
        // Same single locked statement as a stock adjustment, so a sale at the same moment is never lost.
        let changed: Option<(i32, i32, String)> = sqlx::query_as(
            "WITH old AS (SELECT stock FROM variants WHERE id = $1 FOR UPDATE)
             UPDATE variants SET stock = old.stock + $2, updated_at = now()
               FROM old WHERE variants.id = $1
             RETURNING old.stock, variants.stock, variants.product_id",
        )
        .bind(&l.variant_id)
        .bind(l.qty)
        .fetch_optional(&mut *tx)
        .await?;
        let (before, after, product_id) = changed.ok_or_else(|| {
            AppError::invalid(format!(
                "Item {}: that product size no longer exists.",
                i + 1
            ))
        })?;
        sqlx::query(
            "INSERT INTO inventory_txns (variant_id, product_id, type, qty, before_qty, after_qty, reason, by_whom,
                                         receipt_id, batch_no, mfg_date, expiry_date, unit_cost)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(&l.variant_id)
        .bind(&product_id)
        .bind(&req.txn_type)
        .bind(after - before)
        .bind(before)
        .bind(after)
        .bind(&reason)
        .bind(&admin.name)
        .bind(receipt_id)
        .bind(&l.batch_no)
        .bind(l.mfg_date)
        .bind(l.expiry_date)
        .bind(l.unit_cost)
        .execute(&mut *tx)
        .await?;
        lines.push(json!({
            "variantId": l.variant_id, "productId": product_id, "qty": after - before,
            "before": before, "after": after, "batchNo": l.batch_no, "expiryDate": l.expiry_date,
        }));
    }
    let units: i64 = req.lines.iter().map(|l| i64::from(l.qty)).sum();
    let result = json!({
        "receiptId": receipt_id, "type": req.txn_type, "supplierRef": req.supplier_ref,
        "units": units, "lines": lines,
    });
    sqlx::query("UPDATE inventory_receipts SET result = $2 WHERE id = $1")
        .bind(receipt_id)
        .bind(&result)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    audit::record(
        &s.db,
        "admin",
        Some(admin.id),
        "inventory.receipt",
        Some(&receipt_id.to_string()),
        json!({ "lines": req.lines.len(), "units": units, "type": req.txn_type }),
    )
    .await;
    Ok(Json(result))
}

/* ───────────────────────── Expiring soon ───────────────────────── */

#[derive(Deserialize)]
struct ExpiringQuery {
    days: Option<u32>,
}

#[derive(sqlx::FromRow)]
struct ExpiringRow {
    product_id: String,
    name: String,
    variant_id: String,
    size: String,
    barcode: Option<String>,
    stock: i32,
    batch_no: Option<String>,
    expiry_date: NaiveDate,
    received: i64,
}

/// `GET /admin/inventory/expiring?days=60`: received batches that expire within `days` (or already
/// have) for sizes still in stock, soonest first. Stock is counted per size, not per batch, so the
/// list says what may still be on the shelf, for a person to check.
async fn expiring(
    State(s): State<AppState>,
    _admin: AdminAuth,
    Query(q): Query<ExpiringQuery>,
) -> Result<Json<Vec<Value>>> {
    let days = q.days.unwrap_or(60).clamp(1, 365);
    let until = today_ist()
        .checked_add_days(Days::new(u64::from(days)))
        .ok_or_else(|| AppError::invalid("bad number of days"))?;
    let rows: Vec<ExpiringRow> = sqlx::query_as(
        "SELECT p.id AS product_id, p.name, v.id AS variant_id, v.size, v.barcode, v.stock,
                t.batch_no, t.expiry_date, SUM(t.qty)::bigint AS received
           FROM inventory_txns t
           JOIN variants v ON v.id = t.variant_id
           JOIN products p ON p.id = v.product_id
          WHERE t.expiry_date IS NOT NULL AND t.expiry_date <= $1 AND v.stock > 0 AND t.qty > 0
          GROUP BY p.id, p.name, v.id, v.size, v.barcode, v.stock, t.batch_no, t.expiry_date
          ORDER BY t.expiry_date, p.name
          LIMIT 200",
    )
    .bind(until)
    .fetch_all(&s.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                json!({
                    "productId": r.product_id, "name": r.name, "variantId": r.variant_id, "size": r.size,
                    "barcode": r.barcode, "stock": r.stock, "batchNo": r.batch_no,
                    "expiryDate": r.expiry_date, "received": r.received,
                })
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn line(qty: i32) -> ReceiptLine {
        ReceiptLine {
            variant_id: "v1".into(),
            qty,
            batch_no: Some("  B-2610 \n".into()),
            mfg_date: None,
            expiry_date: None,
            unit_cost: None,
        }
    }

    fn receipt(lines: Vec<ReceiptLine>) -> Receipt {
        Receipt {
            txn_type: "addition".into(),
            supplier_ref: "  INV-9 ".into(),
            note: String::new(),
            lines,
        }
    }

    #[test]
    fn codes_look_like_barcodes_or_skus() {
        assert!(valid_code("8901234567890"));
        assert!(valid_code("WG-CR-3K"));
        assert!(!valid_code(""));
        assert!(!valid_code("a b"));
        assert!(!valid_code("../etc"));
        assert!(!valid_code(&"1".repeat(65)));
    }

    #[test]
    fn a_good_receipt_is_tidied() {
        let mut r = receipt(vec![line(12)]);
        check(&mut r, d("2026-10-11")).unwrap();
        assert_eq!(r.supplier_ref, "INV-9");
        assert_eq!(r.lines[0].batch_no.as_deref(), Some("B-2610"));
    }

    #[test]
    fn bad_quantities_and_sizes_are_refused() {
        let today = d("2026-10-11");
        assert!(check(&mut receipt(vec![]), today).is_err());
        assert!(check(&mut receipt(vec![line(0)]), today).is_err());
        assert!(check(&mut receipt(vec![line(-3)]), today).is_err());
        assert!(check(&mut receipt(vec![line(MAX_QTY + 1)]), today).is_err());
        let too_many = (0..=MAX_LINES).map(|_| line(1)).collect();
        assert!(check(&mut receipt(too_many), today).is_err());
        let mut odd = receipt(vec![line(1)]);
        odd.txn_type = "sale".into();
        assert!(check(&mut odd, today).is_err());
    }

    #[test]
    fn dates_must_make_sense() {
        let today = d("2026-10-11");
        let mut expired = receipt(vec![line(1)]);
        expired.lines[0].expiry_date = Some(d("2026-10-10"));
        assert!(check(&mut expired, today).is_err());

        let mut future_mfg = receipt(vec![line(1)]);
        future_mfg.lines[0].mfg_date = Some(d("2026-10-12"));
        assert!(check(&mut future_mfg, today).is_err());

        let mut backwards = receipt(vec![line(1)]);
        backwards.lines[0].mfg_date = Some(d("2026-09-01"));
        backwards.lines[0].expiry_date = Some(d("2026-08-01"));
        assert!(check(&mut backwards, d("2026-07-01")).is_err());

        let mut fine = receipt(vec![line(1)]);
        fine.lines[0].mfg_date = Some(d("2026-09-01"));
        fine.lines[0].expiry_date = Some(d("2027-03-31"));
        assert!(check(&mut fine, today).is_ok());
    }

    #[test]
    fn same_receipt_hashes_the_same() {
        let a = serde_json::to_vec(&receipt(vec![line(2)])).unwrap();
        let b = serde_json::to_vec(&receipt(vec![line(2)])).unwrap();
        let c = serde_json::to_vec(&receipt(vec![line(3)])).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
