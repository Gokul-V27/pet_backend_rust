//! Public catalogue endpoints for shoppers and the customer website:
//! categories, active products, product details, approved reviews,
//! active offers, and active sample campaigns.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    catalogue::*,
    error::{AppError, Result},
    state::AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/categories", get(list_active_categories))
        .route("/products", get(list_active_products))
        .route("/products/{id_or_slug}", get(get_active_product))
        .route("/offers", get(list_active_offers))
        .route("/reviews", get(list_approved_reviews))
        .route("/coupons/validate", post(validate_coupon))
        .route("/samples/campaign", get(get_active_campaign))
}

// ─── Categories ───

async fn list_active_categories(State(s): State<AppState>) -> Result<Json<Vec<Category>>> {
    let rows = sqlx::query_as::<_, Category>(
        "SELECT slug, label, blurb, photo, tone, parent_slug, active, sort
         FROM categories WHERE active = TRUE ORDER BY sort, slug",
    )
    .fetch_all(&s.db)
    .await?;
    Ok(Json(rows))
}

// ─── Products ───

async fn list_active_products(State(s): State<AppState>) -> Result<Json<Vec<Product>>> {
    let rows = sqlx::query_as::<_, ProductRow>(
        "SELECT id, slug, name, brand, species, category_slug, subcategory, life_stage, breed_size, diet, grain_free,
                allergens, summary, description, ingredients, nutrition, best_before, country_of_origin, images,
                benefits, videos, suitable_breeds, feeding_instructions, status, popularity, autoship_eligible,
                gst_rate_pct, hsn, is_new
         FROM products WHERE status = 'active' ORDER BY popularity DESC, created_at DESC",
    )
    .fetch_all(&s.db)
    .await?;

    let variants = sqlx::query_as::<_, VariantRow>(
        "SELECT id, product_id, sku, barcode, stock, low_stock_at, size, weight_kg, price, mrp
         FROM variants ORDER BY price",
    )
    .fetch_all(&s.db)
    .await?;

    let created_ats: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, to_char(created_at, 'YYYY-MM-DD') FROM products WHERE status = 'active'",
    )
    .fetch_all(&s.db)
    .await?;
    let created_map: std::collections::HashMap<String, String> =
        created_ats.into_iter().collect();

    let mut variant_map: std::collections::HashMap<String, Vec<Variant>> =
        std::collections::HashMap::new();
    for v in variants {
        variant_map
            .entry(v.product_id.clone())
            .or_default()
            .push(v.into());
    }

    let products: Vec<Product> = rows
        .into_iter()
        .map(|row| {
            let vs = variant_map.remove(&row.id).unwrap_or_default();
            let ca = created_map
                .get(&row.id)
                .cloned()
                .unwrap_or_else(|| "2026-01-01".into());
            row.into_product(vs, ca)
        })
        .collect();

    Ok(Json(products))
}

async fn get_active_product(
    State(s): State<AppState>,
    Path(id_or_slug): Path<String>,
) -> Result<Json<Product>> {
    let row = sqlx::query_as::<_, ProductRow>(
        "SELECT id, slug, name, brand, species, category_slug, subcategory, life_stage, breed_size, diet, grain_free,
                allergens, summary, description, ingredients, nutrition, best_before, country_of_origin, images,
                benefits, videos, suitable_breeds, feeding_instructions, status, popularity, autoship_eligible,
                gst_rate_pct, hsn, is_new
         FROM products WHERE (id = $1 OR slug = $1) AND status = 'active'",
    )
    .bind(&id_or_slug)
    .fetch_optional(&s.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let variants = sqlx::query_as::<_, VariantRow>(
        "SELECT id, product_id, sku, barcode, stock, low_stock_at, size, weight_kg, price, mrp
         FROM variants WHERE product_id = $1 ORDER BY price",
    )
    .bind(&row.id)
    .fetch_all(&s.db)
    .await?
    .into_iter()
    .map(Variant::from)
    .collect();

    let ca: (String,) = sqlx::query_as(
        "SELECT to_char(created_at, 'YYYY-MM-DD') FROM products WHERE id = $1",
    )
    .bind(&row.id)
    .fetch_one(&s.db)
    .await?;

    Ok(Json(row.into_product(variants, ca.0)))
}

// ─── Offers ───

async fn list_active_offers(State(s): State<AppState>) -> Result<Json<Vec<Value>>> {
    let rows: Vec<OfferRow> = sqlx::query_as(
        "SELECT id, kind, title, line, image, badge, link, cta, coupon_code, species, food, flash,
                starts_at, ends_at, active, in_loader, poster_line, poster_big, poster_image
         FROM offers WHERE active = TRUE ORDER BY created_at DESC",
    )
    .fetch_all(&s.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|o| serde_json::to_value(o).unwrap_or_default())
            .collect(),
    ))
}

// ─── Reviews ───

#[derive(Deserialize)]
struct ReviewsQuery {
    product_id: Option<String>,
}

async fn list_approved_reviews(
    State(s): State<AppState>,
    Query(q): Query<ReviewsQuery>,
) -> Result<Json<Vec<Review>>> {
    let rows: Vec<ReviewRow> = if let Some(ref pid) = q.product_id {
        sqlx::query_as(
            "SELECT id, product_id, author, pet_label, pet_species, rating, title, body, date, helpful, order_id, reply, media, status, featured
             FROM reviews WHERE status = 'approved' AND product_id = $1 ORDER BY featured DESC, rating DESC, created_at DESC",
        )
        .bind(pid)
        .fetch_all(&s.db)
        .await?
    } else {
        sqlx::query_as(
            "SELECT id, product_id, author, pet_label, pet_species, rating, title, body, date, helpful, order_id, reply, media, status, featured
             FROM reviews WHERE status = 'approved' ORDER BY featured DESC, created_at DESC LIMIT 100",
        )
        .fetch_all(&s.db)
        .await?
    };
    Ok(Json(rows.into_iter().map(Review::from).collect()))
}

// ─── Coupons ───

#[derive(Deserialize)]
struct ValidateCouponInput {
    code: String,
}

async fn validate_coupon(
    State(s): State<AppState>,
    Json(input): Json<ValidateCouponInput>,
) -> Result<Json<Value>> {
    let code = input.code.trim().to_uppercase();
    if code.is_empty() {
        return Err(AppError::invalid("coupon code is required"));
    }

    let coupon: Option<CouponRow> = sqlx::query_as(
        "SELECT code, title, description, kind, value, max_discount, min_order, first_order_only,
                categories, product_ids, excludes_autoship, starts_at, ends_at, usage_limit, per_customer_limit, used, active
         FROM coupons WHERE upper(code) = $1 AND active = TRUE",
    )
    .bind(&code)
    .fetch_optional(&s.db)
    .await?;

    let Some(c) = coupon else {
        return Err(AppError::invalid("invalid or expired coupon"));
    };

    if let Some(limit) = c.usage_limit {
        if c.used >= limit {
            return Err(AppError::invalid("this coupon has reached its usage limit"));
        }
    }

    let coupon_obj = Coupon::from(c);
    Ok(Json(json!({
        "valid": true,
        "coupon": coupon_obj,
    })))
}

// ─── Sample Campaign ───

async fn get_active_campaign(State(s): State<AppState>) -> Result<Json<Value>> {
    let row: Option<SampleCampaignRow> = sqlx::query_as(
        "SELECT id, name, product_id, size, starts_at, ends_at, max_claims, claims, per_household, delivery_fee, first_order_only, active
         FROM sample_campaigns WHERE active = TRUE LIMIT 1",
    )
    .fetch_optional(&s.db)
    .await?;

    match row {
        Some(r) => Ok(Json(serde_json::to_value(r).unwrap_or_default())),
        None => Ok(Json(json!(null))),
    }
}
