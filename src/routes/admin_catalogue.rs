//! Admin CRUD endpoints for the catalogue: categories, products, variants,
//! reviews, coupons, offers, inventory, samples, message templates, care
//! templates, settings, and audit log.

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post, put},
};
use serde_json::{Value, json};

use crate::{
    auth::AdminAuth,
    catalogue::*,
    error::{AppError, Result},
    state::AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        // ── Categories ──
        .route("/categories", get(list_categories).post(save_category))
        .route("/categories/{slug}", put(save_category_by_slug))
        // ── Products ──
        .route("/products", get(list_products).post(save_product))
        .route("/products/{id}", get(get_product).put(update_product))
        // ── Reviews ──
        .route("/reviews", get(list_reviews))
        .route("/reviews/{id}", put(moderate_review))
        // ── Coupons ──
        .route("/coupons", get(list_coupons).post(save_coupon))
        .route("/coupons/{code}", put(update_coupon).delete(delete_coupon))
        // ── Offers ──
        .route("/offers", get(list_offers).post(save_offer))
        .route("/offers/{id}", put(update_offer).delete(delete_offer_by_id))
        // ── Inventory ──
        .route("/inventory/txns", get(list_inventory_txns))
        .route("/inventory/adjust", post(adjust_stock))
        // ── Sample campaigns ──
        .route("/samples/campaign", get(get_campaign).put(save_campaign))
        // ── Message templates ──
        .route("/messages/templates", get(list_msg_templates))
        .route("/messages/templates/{id}", put(save_msg_template))
        // ── Care templates ──
        .route(
            "/care-templates",
            get(list_care_templates).post(save_care_template),
        )
        .route(
            "/care-templates/{id}",
            put(update_care_template).delete(delete_care_template_by_id),
        )
        // ── Settings ──
        .route("/settings", get(get_settings).put(save_settings))
        // ── Audit log ──
        .route("/audit", get(list_audit))
}

// ═════════════════════════ Categories ═════════════════════════

async fn list_categories(
    State(s): State<AppState>,
    _admin: AdminAuth,
) -> Result<Json<Vec<Category>>> {
    let rows = sqlx::query_as::<_, Category>("SELECT slug, label, blurb, photo, tone, parent_slug, active, sort FROM categories ORDER BY sort, slug")
        .fetch_all(&s.db).await?;
    Ok(Json(rows))
}

async fn save_category(
    State(s): State<AppState>,
    admin: AdminAuth,
    Json(c): Json<CategoryInput>,
) -> Result<Json<Category>> {
    allow(admin.role.can_edit_catalogue())?;
    if c.slug.is_empty() || c.label.is_empty() {
        return Err(AppError::invalid("slug and label are required"));
    }
    let row = sqlx::query_as::<_, Category>(
        "INSERT INTO categories (slug, label, blurb, photo, tone, parent_slug, active, sort)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (slug) DO UPDATE SET label=$2, blurb=$3, photo=$4, tone=$5, parent_slug=$6, active=$7, sort=$8, updated_at=now()
         RETURNING slug, label, blurb, photo, tone, parent_slug, active, sort"
    )
    .bind(&c.slug).bind(&c.label).bind(&c.blurb).bind(&c.photo).bind(&c.tone)
    .bind(&c.parent_slug).bind(c.active).bind(c.sort)
    .fetch_one(&s.db).await?;
    audit(&s, &admin, "save_category", &c.slug).await;
    Ok(Json(row))
}

async fn save_category_by_slug(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(slug): Path<String>,
    Json(mut c): Json<CategoryInput>,
) -> Result<Json<Category>> {
    c.slug = slug;
    save_category(State(s), admin, Json(c)).await
}

// ═════════════════════════ Products ═════════════════════════

async fn list_products(State(s): State<AppState>, _admin: AdminAuth) -> Result<Json<Vec<Product>>> {
    let rows = sqlx::query_as::<_, ProductRow>(
        "SELECT id, slug, name, brand, species, category_slug, subcategory, life_stage, breed_size, diet, grain_free,
                allergens, summary, description, ingredients, nutrition, best_before, country_of_origin, images,
                benefits, videos, suitable_breeds, feeding_instructions, status, popularity, autoship_eligible,
                gst_rate_pct, hsn, is_new
         FROM products ORDER BY created_at DESC"
    ).fetch_all(&s.db).await?;

    let variants = sqlx::query_as::<_, VariantRow>(
        "SELECT id, product_id, sku, barcode, stock, low_stock_at, size, weight_kg, price, mrp FROM variants ORDER BY price"
    ).fetch_all(&s.db).await?;

    let created_ats: Vec<(String, String)> =
        sqlx::query_as("SELECT id, to_char(created_at, 'YYYY-MM-DD') FROM products")
            .fetch_all(&s.db)
            .await?;
    let created_map: std::collections::HashMap<String, String> = created_ats.into_iter().collect();

    let products: Vec<Product> = rows
        .into_iter()
        .map(|p| {
            let pid = p.id.clone();
            let pvariants: Vec<Variant> = variants
                .iter()
                .filter(|v| v.product_id == pid)
                .cloned()
                .map(Variant::from)
                .collect();
            let ca = created_map.get(&pid).cloned().unwrap_or_default();
            p.into_product(pvariants, ca)
        })
        .collect();

    Ok(Json(products))
}

async fn get_product(
    State(s): State<AppState>,
    _admin: AdminAuth,
    Path(id): Path<String>,
) -> Result<Json<Product>> {
    let row = sqlx::query_as::<_, ProductRow>(
        "SELECT id, slug, name, brand, species, category_slug, subcategory, life_stage, breed_size, diet, grain_free,
                allergens, summary, description, ingredients, nutrition, best_before, country_of_origin, images,
                benefits, videos, suitable_breeds, feeding_instructions, status, popularity, autoship_eligible,
                gst_rate_pct, hsn, is_new
         FROM products WHERE id = $1"
    ).bind(&id).fetch_optional(&s.db).await?.ok_or(AppError::NotFound)?;

    let variants: Vec<Variant> = sqlx::query_as::<_, VariantRow>(
        "SELECT id, product_id, sku, barcode, stock, low_stock_at, size, weight_kg, price, mrp FROM variants WHERE product_id = $1 ORDER BY price"
    ).bind(&id).fetch_all(&s.db).await?.into_iter().map(Variant::from).collect();

    let ca: (String,) =
        sqlx::query_as("SELECT to_char(created_at, 'YYYY-MM-DD') FROM products WHERE id = $1")
            .bind(&id)
            .fetch_one(&s.db)
            .await?;

    Ok(Json(row.into_product(variants, ca.0)))
}

async fn save_product(
    State(s): State<AppState>,
    admin: AdminAuth,
    Json(p): Json<Product>,
) -> Result<Json<Product>> {
    upsert_product(&s, &admin, p).await
}

async fn update_product(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(id): Path<String>,
    Json(mut p): Json<Product>,
) -> Result<Json<Product>> {
    p.id = id;
    upsert_product(&s, &admin, p).await
}

async fn upsert_product(s: &AppState, admin: &AdminAuth, p: Product) -> Result<Json<Product>> {
    allow(admin.role.can_edit_catalogue())?;
    if p.id.is_empty() || p.slug.is_empty() || p.name.is_empty() {
        return Err(AppError::invalid("id, slug and name are required"));
    }

    let grain_free = p.grain_free.unwrap_or(false);

    sqlx::query(
        "INSERT INTO products (id, slug, name, brand, species, category_slug, subcategory, life_stage, breed_size, diet, grain_free,
                allergens, summary, description, ingredients, nutrition, best_before, country_of_origin, images,
                benefits, videos, suitable_breeds, feeding_instructions, status, popularity, autoship_eligible,
                gst_rate_pct, hsn, is_new)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25,$26,$27,$28,$29)
         ON CONFLICT (id) DO UPDATE SET
           slug=$2, name=$3, brand=$4, species=$5, category_slug=$6, subcategory=$7, life_stage=$8, breed_size=$9,
           diet=$10, grain_free=$11, allergens=$12, summary=$13, description=$14, ingredients=$15, nutrition=$16,
           best_before=$17, country_of_origin=$18, images=$19, benefits=$20, videos=$21, suitable_breeds=$22,
           feeding_instructions=$23, status=$24, autoship_eligible=$26, gst_rate_pct=$27, hsn=$28,
           is_new=$29, updated_at=now()"
        // (`popularity` is counted from orders; saving a product never resets it.)
    )
    .bind(&p.id).bind(&p.slug).bind(&p.name).bind(&p.brand).bind(&p.species)
    .bind(&p.category).bind(&p.subcategory).bind(&p.life_stage).bind(&p.breed_size)
    .bind(&p.diet).bind(grain_free).bind(&p.allergens)
    .bind(&p.summary).bind(&p.description).bind(&p.ingredients).bind(&p.nutrition)
    .bind(&p.best_before).bind(&p.country_of_origin).bind(&p.images)
    .bind(&p.benefits).bind(&p.videos).bind(&p.suitable_breeds)
    .bind(&p.feeding_instructions).bind(&p.status).bind(p.popularity)
    .bind(p.autoship_eligible).bind(p.gst_rate_pct).bind(&p.hsn).bind(p.is_new)
    .execute(&s.db).await?;

    // Upsert variants
    for v in &p.variants {
        let stock = if v.in_stock && v.stock == 0 {
            1
        } else {
            v.stock
        };
        sqlx::query(
            "INSERT INTO variants (id, product_id, sku, barcode, stock, low_stock_at, size, weight_kg, price, mrp)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
             -- Stock is not part of the update: it changes only through orders and inventory adjustments,
             -- so saving a product from a stale screen can never overwrite what customers just bought.
             ON CONFLICT (id) DO UPDATE SET sku=$3, barcode=$4, low_stock_at=$6, size=$7, weight_kg=$8, price=$9, mrp=$10, updated_at=now()"
        )
        .bind(&v.id).bind(&p.id).bind(&v.sku).bind(&v.barcode)
        .bind(stock).bind(v.low_stock_at).bind(&v.size).bind(v.weight_kg)
        .bind(v.price).bind(v.mrp)
        .execute(&s.db).await?;
    }

    // Remove variants that are no longer in the list
    let variant_ids: Vec<&str> = p.variants.iter().map(|v| v.id.as_str()).collect();
    if !variant_ids.is_empty() {
        sqlx::query("DELETE FROM variants WHERE product_id = $1 AND NOT (id = ANY($2))")
            .bind(&p.id)
            .bind(&variant_ids)
            .execute(&s.db)
            .await?;
    }

    audit(s, admin, "save_product", &p.id).await;
    get_product(State(s.clone()), admin.clone(), Path(p.id)).await
}

// ═════════════════════════ Reviews ═════════════════════════

async fn list_reviews(State(s): State<AppState>, _admin: AdminAuth) -> Result<Json<Vec<Review>>> {
    let rows: Vec<ReviewRow> = sqlx::query_as(
        "SELECT id, product_id, author, pet_label, pet_species, rating, title, body, date, helpful, order_id, reply, media, status, featured
         FROM reviews ORDER BY created_at DESC"
    ).fetch_all(&s.db).await?;
    Ok(Json(rows.into_iter().map(Review::from).collect()))
}

async fn moderate_review(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(id): Path<String>,
    Json(patch): Json<Value>,
) -> Result<Json<Review>> {
    allow(admin.role.can_run_marketing())?;
    if let Some(status) = patch.get("status").and_then(|v| v.as_str()) {
        if !["pending", "approved", "rejected", "hidden"].contains(&status) {
            return Err(AppError::invalid("unknown review status"));
        }
        sqlx::query("UPDATE reviews SET status = $2 WHERE id = $1")
            .bind(&id)
            .bind(status)
            .execute(&s.db)
            .await?;
    }
    if let Some(featured) = patch.get("featured").and_then(|v| v.as_bool()) {
        sqlx::query("UPDATE reviews SET featured = $2 WHERE id = $1")
            .bind(&id)
            .bind(featured)
            .execute(&s.db)
            .await?;
    }
    if let Some(reply) = patch.get("reply") {
        sqlx::query("UPDATE reviews SET reply = $2 WHERE id = $1")
            .bind(&id)
            .bind(reply)
            .execute(&s.db)
            .await?;
    }
    let row: ReviewRow = sqlx::query_as(
        "SELECT id, product_id, author, pet_label, pet_species, rating, title, body, date, helpful, order_id, reply, media, status, featured FROM reviews WHERE id = $1"
    ).bind(&id).fetch_optional(&s.db).await?.ok_or(AppError::NotFound)?;
    audit(&s, &admin, "moderate_review", &id).await;
    Ok(Json(row.into()))
}

// ═════════════════════════ Coupons ═════════════════════════

async fn list_coupons(State(s): State<AppState>, _admin: AdminAuth) -> Result<Json<Vec<Coupon>>> {
    let rows: Vec<CouponRow> = sqlx::query_as(
        "SELECT code, title, description, kind, value, max_discount, min_order, first_order_only,
                categories, product_ids, excludes_autoship, starts_at, ends_at, usage_limit, per_customer_limit, used, active
         FROM coupons ORDER BY created_at DESC"
    ).fetch_all(&s.db).await?;
    Ok(Json(rows.into_iter().map(Coupon::from).collect()))
}

async fn save_coupon(
    State(s): State<AppState>,
    admin: AdminAuth,
    Json(c): Json<Coupon>,
) -> Result<Json<Coupon>> {
    upsert_coupon(&s, &admin, c).await
}

async fn update_coupon(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(code): Path<String>,
    Json(mut c): Json<Coupon>,
) -> Result<Json<Coupon>> {
    c.code = code;
    upsert_coupon(&s, &admin, c).await
}

async fn upsert_coupon(s: &AppState, admin: &AdminAuth, c: Coupon) -> Result<Json<Coupon>> {
    allow(admin.role.can_run_marketing())?;
    if c.code.is_empty() || c.title.is_empty() {
        return Err(AppError::invalid("code and title are required"));
    }
    sqlx::query(
        "INSERT INTO coupons (code, title, description, kind, value, max_discount, min_order, first_order_only,
                categories, product_ids, excludes_autoship, starts_at, ends_at, usage_limit, per_customer_limit, used, active)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)
         ON CONFLICT (code) DO UPDATE SET
           title=$2, description=$3, kind=$4, value=$5, max_discount=$6, min_order=$7, first_order_only=$8,
           categories=$9, product_ids=$10, excludes_autoship=$11, starts_at=$12, ends_at=$13, usage_limit=$14,
           per_customer_limit=$15, active=$17, updated_at=now()"
        // (`used` is counted by orders; saving a coupon never resets it.)
    )
    .bind(&c.code).bind(&c.title).bind(&c.description).bind(&c.kind).bind(c.value)
    .bind(c.max_discount).bind(c.min_order).bind(c.first_order_only)
    .bind(&c.categories).bind(&c.product_ids).bind(c.excludes_autoship_lines)
    .bind(&c.starts_at).bind(&c.ends_at).bind(c.usage_limit).bind(c.per_customer_limit)
    .bind(c.used).bind(c.active)
    .execute(&s.db).await?;
    audit(s, admin, "save_coupon", &c.code).await;
    Ok(Json(c))
}

async fn delete_coupon(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(code): Path<String>,
) -> Result<Json<Value>> {
    allow(admin.role.can_run_marketing())?;
    sqlx::query("DELETE FROM coupons WHERE code = $1")
        .bind(&code)
        .execute(&s.db)
        .await?;
    audit(&s, &admin, "delete_coupon", &code).await;
    Ok(Json(json!({ "deleted": code })))
}

// ═════════════════════════ Offers ═════════════════════════

async fn list_offers(State(s): State<AppState>, _admin: AdminAuth) -> Result<Json<Vec<Value>>> {
    let rows: Vec<OfferRow> = sqlx::query_as(
        "SELECT id, kind, title, line, image, badge, link, cta, coupon_code, species, food, flash,
                starts_at, ends_at, active, in_loader, poster_line, poster_big, poster_image
         FROM offers ORDER BY created_at DESC",
    )
    .fetch_all(&s.db)
    .await?;
    // Return as generic JSON to match the admin website shape exactly
    Ok(Json(
        rows.into_iter()
            .map(|o| serde_json::to_value(o).unwrap_or_default())
            .collect(),
    ))
}

async fn save_offer(
    State(s): State<AppState>,
    admin: AdminAuth,
    Json(o): Json<Value>,
) -> Result<Json<Value>> {
    let id = o
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if id.is_empty() {
        return Err(AppError::invalid("id is required"));
    }
    upsert_offer_value(&s, &admin, &o).await?;
    Ok(Json(o))
}

async fn update_offer(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(_id): Path<String>,
    Json(o): Json<Value>,
) -> Result<Json<Value>> {
    upsert_offer_value(&s, &admin, &o).await?;
    Ok(Json(o))
}

async fn upsert_offer_value(s: &AppState, admin: &AdminAuth, o: &Value) -> Result<()> {
    allow(admin.role.can_run_marketing())?;
    let str_field = |k: &str| o.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let bool_field = |k: &str| o.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let opt_str = |k: &str| o.get(k).and_then(|v| v.as_str()).map(String::from);

    let id = str_field("id");
    sqlx::query(
        "INSERT INTO offers (id, kind, title, line, image, badge, link, cta, coupon_code, species, food, flash,
                starts_at, ends_at, active, in_loader, poster_line, poster_big, poster_image)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19)
         ON CONFLICT (id) DO UPDATE SET
           kind=$2, title=$3, line=$4, image=$5, badge=$6, link=$7, cta=$8, coupon_code=$9, species=$10,
           food=$11, flash=$12, starts_at=$13, ends_at=$14, active=$15, in_loader=$16, poster_line=$17,
           poster_big=$18, poster_image=$19, updated_at=now()"
    )
    .bind(&id).bind(str_field("kind")).bind(str_field("title")).bind(str_field("line"))
    .bind(str_field("image")).bind(str_field("badge")).bind(str_field("link")).bind(str_field("cta"))
    .bind(opt_str("couponCode")).bind(str_field("species"))
    .bind(bool_field("food")).bind(bool_field("flash"))
    .bind(str_field("startsAt")).bind(opt_str("endsAt"))
    .bind(bool_field("active")).bind(bool_field("inLoader"))
    .bind(opt_str("posterLine")).bind(opt_str("posterBig")).bind(opt_str("posterImage"))
    .execute(&s.db).await?;
    audit(s, admin, "save_offer", &id).await;
    Ok(())
}

async fn delete_offer_by_id(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    allow(admin.role.can_run_marketing())?;
    sqlx::query("DELETE FROM offers WHERE id = $1")
        .bind(&id)
        .execute(&s.db)
        .await?;
    audit(&s, &admin, "delete_offer", &id).await;
    Ok(Json(json!({ "deleted": id })))
}

// ═════════════════════════ Inventory ═════════════════════════

async fn list_inventory_txns(
    State(s): State<AppState>,
    _admin: AdminAuth,
) -> Result<Json<Vec<InventoryTxn>>> {
    let rows: Vec<(String, String, String, String, i32, i32, i32, String, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT id, variant_id, product_id, type, qty, before_qty, after_qty, reason, by_whom, created_at
         FROM inventory_txns ORDER BY created_at DESC LIMIT 500"
    ).fetch_all(&s.db).await?;
    let txns = rows
        .into_iter()
        .map(|r| InventoryTxn {
            id: r.0,
            variant_id: r.1,
            product_id: r.2,
            txn_type: r.3,
            qty: r.4,
            before: r.5,
            after: r.6,
            reason: r.7,
            by: r.8,
            at: r.9.to_rfc3339(),
        })
        .collect();
    Ok(Json(txns))
}

async fn adjust_stock(
    State(s): State<AppState>,
    admin: AdminAuth,
    Json(input): Json<AdjustStockInput>,
) -> Result<Json<Value>> {
    allow(admin.role.can_adjust_stock())?;
    const TYPES: [&str; 5] = ["opening", "addition", "sale", "return", "adjustment"];
    if !TYPES.contains(&input.txn_type.as_str()) {
        return Err(AppError::invalid("unknown stock change type"));
    }
    // Either a change (+/-) or, for a stock count, the number actually on the shelf.
    match input.counted {
        Some(n) if !(0..=1_000_000).contains(&n) => {
            return Err(AppError::invalid("a stock count must be between 0 and 1000000"));
        }
        None if input.qty == 0 || input.qty.abs() > 100_000 => {
            return Err(AppError::invalid("quantity must be between -100000 and 100000, not zero"));
        }
        _ => {}
    }
    let reason: String = input.reason.trim().chars().take(200).collect();
    // One transaction, one statement for the stock change: the row is locked, read and updated
    // together, so an order placed at the same moment can never be wiped out by this adjustment.
    // A count sets the stock to what was counted against the live number, not the screen's copy.
    let mut tx = s.db.begin().await?;
    let changed: Option<(i32, i32, String)> = sqlx::query_as(
        "WITH old AS (SELECT stock FROM variants WHERE id = $1 FOR UPDATE)
         UPDATE variants SET stock = COALESCE($3, GREATEST(old.stock + $2, 0)), updated_at = now()
           FROM old WHERE variants.id = $1
         RETURNING old.stock, variants.stock, variants.product_id",
    )
    .bind(&input.variant_id)
    .bind(input.qty)
    .bind(input.counted)
    .fetch_optional(&mut *tx)
    .await?;
    let (before, after, product_id) = changed.ok_or(AppError::NotFound)?;
    if before == after {
        // A count that matched, or a write-off of stock that was already zero: nothing to record.
        tx.commit().await?;
        return Ok(Json(json!({ "before": before, "after": after })));
    }

    // The log keeps the change that really happened (a write-off stops at zero).
    sqlx::query(
        "INSERT INTO inventory_txns (variant_id, product_id, type, qty, before_qty, after_qty, reason, by_whom)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"
    )
    .bind(&input.variant_id).bind(&product_id).bind(&input.txn_type)
    .bind(after - before).bind(before).bind(after).bind(&reason).bind(&admin.name)
    .execute(&mut *tx).await?;
    tx.commit().await?;

    Ok(Json(json!({ "before": before, "after": after })))
}

// ═════════════════════════ Sample campaigns ═════════════════════════

async fn get_campaign(State(s): State<AppState>, _admin: AdminAuth) -> Result<Json<Value>> {
    let row: Option<SampleCampaignRow> = sqlx::query_as(
        "SELECT id, name, product_id, size, starts_at, ends_at, max_claims, claims, per_household, delivery_fee, first_order_only, active
         FROM sample_campaigns LIMIT 1"
    ).fetch_optional(&s.db).await?;
    match row {
        Some(c) => Ok(Json(serde_json::to_value(c).unwrap_or_default())),
        None => Ok(Json(json!({}))),
    }
}

async fn save_campaign(
    State(s): State<AppState>,
    admin: AdminAuth,
    Json(c): Json<Value>,
) -> Result<Json<Value>> {
    allow(admin.role.can_run_marketing())?;
    let str_field = |k: &str| c.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let int_field = |k: &str| c.get(k).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    let bool_field = |k: &str| c.get(k).and_then(|v| v.as_bool()).unwrap_or(false);

    let id = str_field("id");
    if id.is_empty() {
        return Err(AppError::invalid("campaign id is required"));
    }
    // `claims` is counted by the shop; saving the campaign never resets it.
    sqlx::query(
        "INSERT INTO sample_campaigns (id, name, product_id, size, starts_at, ends_at, max_claims, claims, per_household, delivery_fee, first_order_only, active)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
         ON CONFLICT (id) DO UPDATE SET
           name=$2, product_id=$3, size=$4, starts_at=$5, ends_at=$6, max_claims=$7,
           per_household=$9, delivery_fee=$10, first_order_only=$11, active=$12, updated_at=now()"
    )
    .bind(&id).bind(str_field("name")).bind(str_field("productId")).bind(str_field("size"))
    .bind(str_field("startsAt")).bind(str_field("endsAt")).bind(int_field("maxClaims"))
    .bind(int_field("claims")).bind(int_field("perHousehold")).bind(int_field("deliveryFee"))
    .bind(bool_field("firstOrderOnly")).bind(bool_field("active"))
    .execute(&s.db).await?;
    audit(&s, &admin, "save_campaign", &id).await;
    Ok(Json(c))
}

// ═════════════════════════ Message templates ═════════════════════════

async fn list_msg_templates(
    State(s): State<AppState>,
    _admin: AdminAuth,
) -> Result<Json<Vec<MessageTemplateRow>>> {
    let rows = sqlx::query_as::<_, MessageTemplateRow>(
        "SELECT id, name, event, channel, body, approval, active FROM message_templates ORDER BY id"
    ).fetch_all(&s.db).await?;
    Ok(Json(rows))
}

async fn save_msg_template(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(id): Path<String>,
    Json(t): Json<Value>,
) -> Result<Json<Value>> {
    allow(admin.role.can_edit_messages())?;
    let str_field = |k: &str| t.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let bool_field = |k: &str| t.get(k).and_then(|v| v.as_bool()).unwrap_or(false);

    sqlx::query(
        "INSERT INTO message_templates (id, name, event, channel, body, approval, active)
         VALUES ($1,$2,$3,$4,$5,$6,$7)
         ON CONFLICT (id) DO UPDATE SET name=$2, event=$3, channel=$4, body=$5, approval=$6, active=$7, updated_at=now()"
    )
    .bind(&id).bind(str_field("name")).bind(str_field("event")).bind(str_field("channel"))
    .bind(str_field("body")).bind(str_field("approval")).bind(bool_field("active"))
    .execute(&s.db).await?;
    audit(&s, &admin, "save_msg_template", &id).await;
    Ok(Json(t))
}

// ═════════════════════════ Care templates ═════════════════════════

async fn list_care_templates(
    State(s): State<AppState>,
    _admin: AdminAuth,
) -> Result<Json<Vec<CareTemplateRow>>> {
    let rows = sqlx::query_as::<_, CareTemplateRow>(
        "SELECT id, kind, name, species, first_due_weeks, repeat_months, remind_days_before, channels, message, active
         FROM care_templates ORDER BY kind, name"
    ).fetch_all(&s.db).await?;
    Ok(Json(rows))
}

async fn save_care_template(
    State(s): State<AppState>,
    admin: AdminAuth,
    Json(t): Json<Value>,
) -> Result<Json<Value>> {
    let id = t
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    upsert_care_template_value(&s, &admin, &id, &t).await?;
    Ok(Json(t))
}

async fn update_care_template(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(id): Path<String>,
    Json(t): Json<Value>,
) -> Result<Json<Value>> {
    upsert_care_template_value(&s, &admin, &id, &t).await?;
    Ok(Json(t))
}

async fn upsert_care_template_value(
    s: &AppState,
    admin: &AdminAuth,
    id: &str,
    t: &Value,
) -> Result<()> {
    allow(admin.role.can_run_marketing())?;
    let str_field = |k: &str| t.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let int_field = |k: &str| t.get(k).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    let bool_field = |k: &str| t.get(k).and_then(|v| v.as_bool()).unwrap_or(true);
    let arr_field = |k: &str| -> Vec<String> {
        t.get(k)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };

    let species = arr_field("species");
    let channels = arr_field("channels");

    sqlx::query(
        "INSERT INTO care_templates (id, kind, name, species, first_due_weeks, repeat_months, remind_days_before, channels, message, active)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
         ON CONFLICT (id) DO UPDATE SET kind=$2, name=$3, species=$4, first_due_weeks=$5, repeat_months=$6,
           remind_days_before=$7, channels=$8, message=$9, active=$10, updated_at=now()"
    )
    .bind(id).bind(str_field("kind")).bind(str_field("name")).bind(&species)
    .bind(int_field("firstDueWeeks")).bind(int_field("repeatMonths"))
    .bind(int_field("remindDaysBefore")).bind(&channels).bind(str_field("message"))
    .bind(bool_field("active"))
    .execute(&s.db).await?;
    audit(s, admin, "save_care_template", id).await;
    Ok(())
}

async fn delete_care_template_by_id(
    State(s): State<AppState>,
    admin: AdminAuth,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    allow(admin.role.can_run_marketing())?;
    sqlx::query("DELETE FROM care_templates WHERE id = $1")
        .bind(&id)
        .execute(&s.db)
        .await?;
    audit(&s, &admin, "delete_care_template", &id).await;
    Ok(Json(json!({ "deleted": id })))
}

// ═════════════════════════ Settings ═════════════════════════

async fn get_settings(State(s): State<AppState>, _admin: AdminAuth) -> Result<Json<Value>> {
    let rows: Vec<AdminSettingRow> = sqlx::query_as("SELECT key, value FROM admin_settings")
        .fetch_all(&s.db)
        .await?;
    let mut map = serde_json::Map::new();
    for r in rows {
        map.insert(r.key, r.value);
    }
    Ok(Json(Value::Object(map)))
}

async fn save_settings(
    State(s): State<AppState>,
    admin: AdminAuth,
    Json(body): Json<Value>,
) -> Result<Json<Value>> {
    allow(admin.role.can_run_marketing())?;
    const KEYS: [&str; 8] = ["donation", "referral", "share", "themes", "loader", "supplementIds", "urgentDays", "chatbot"];
    let Value::Object(map) = &body else {
        return Err(AppError::invalid("settings must be an object"));
    };
    if let Some(bad) = map.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(AppError::invalid(format!("unknown setting {bad}")));
    }
    {
        for (key, value) in map {
            sqlx::query(
                "INSERT INTO admin_settings (key, value) VALUES ($1, $2)
                 ON CONFLICT (key) DO UPDATE SET value = $2, updated_at = now()",
            )
            .bind(key)
            .bind(value)
            .execute(&s.db)
            .await?;
        }
    }
    audit(&s, &admin, "save_settings", "").await;
    Ok(Json(body))
}

// ═════════════════════════ Audit log ═════════════════════════

async fn list_audit(State(s): State<AppState>, admin: AdminAuth) -> Result<Json<Vec<Value>>> {
    allow(admin.role.can_view_audit())?;
    let rows: Vec<(i64, chrono::DateTime<chrono::Utc>, String, Option<uuid::Uuid>, String, Option<String>, Value)> = sqlx::query_as(
        "SELECT l.id, l.at, COALESCE(u.role, l.actor_type), l.actor_id, l.action || COALESCE(' by ' || u.email, ''), l.target, l.detail
           FROM audit_log l LEFT JOIN admin_users u ON l.actor_type = 'admin' AND u.id = l.actor_id
          ORDER BY l.at DESC LIMIT 300"
    ).fetch_all(&s.db).await?;
    let entries: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.0, "at": r.1.to_rfc3339(), "actorType": r.2,
                "actorId": r.3, "action": r.4, "target": r.5, "detail": r.6
            })
        })
        .collect();
    Ok(Json(entries))
}

// ─── Helpers ───

/// Every change is checked against the same permission table the admin website shows.
fn allow(ok: bool) -> Result<()> {
    if ok { Ok(()) } else { Err(AppError::Forbidden) }
}

async fn audit(s: &AppState, admin: &AdminAuth, action: &str, target: &str) {
    let _ = sqlx::query(
        "INSERT INTO audit_log (actor_type, actor_id, action, target) VALUES ('admin', $1, $2, $3)",
    )
    .bind(admin.id)
    .bind(action)
    .bind(target)
    .execute(&s.db)
    .await;
}
