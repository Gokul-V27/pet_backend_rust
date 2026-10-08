//! Catalogue domain types — categories, products, variants, reviews, coupons,
//! offers, inventory, samples, message templates, care templates and settings.

use serde::{Deserialize, Serialize};
use sqlx::FromRow;

// ─── Categories ───

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Category {
    pub slug: String,
    pub label: String,
    pub blurb: String,
    pub photo: String,
    pub tone: String,
    pub parent_slug: Option<String>,
    pub active: bool,
    pub sort: i32,
}

#[derive(Debug, Deserialize)]
pub struct CategoryInput {
    pub slug: String,
    pub label: String,
    #[serde(default)]
    pub blurb: String,
    #[serde(default)]
    pub photo: String,
    #[serde(default = "default_tone")]
    pub tone: String,
    pub parent_slug: Option<String>,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default)]
    pub sort: i32,
}

// ─── Products ───

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ProductRow {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub brand: String,
    pub species: Vec<String>,
    pub category_slug: String,
    pub subcategory: Option<String>,
    pub life_stage: String,
    pub breed_size: String,
    pub diet: Option<String>,
    pub grain_free: bool,
    pub allergens: Vec<String>,
    pub summary: String,
    pub description: String,
    pub ingredients: Vec<String>,
    pub nutrition: Option<serde_json::Value>,
    pub best_before: Option<String>,
    pub country_of_origin: String,
    pub images: Vec<String>,
    pub benefits: Vec<String>,
    pub videos: Vec<String>,
    pub suitable_breeds: Vec<String>,
    pub feeding_instructions: Option<String>,
    pub status: String,
    pub popularity: i32,
    pub autoship_eligible: bool,
    pub gst_rate_pct: i32,
    pub hsn: String,
    pub is_new: bool,
}

/// The shape the admin website sends and receives.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Product {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub brand: String,
    pub species: Vec<String>,
    pub category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subcategory: Option<String>,
    pub life_stage: String,
    pub breed_size: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grain_free: Option<bool>,
    pub allergens: Vec<String>,
    pub summary: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ingredients: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nutrition: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_before: Option<String>,
    pub country_of_origin: String,
    pub images: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub benefits: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub videos: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suitable_breeds: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feeding_instructions: Option<String>,
    pub status: String,
    pub popularity: i32,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    pub autoship_eligible: bool,
    pub gst_rate_pct: i32,
    pub hsn: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_new: bool,
    pub variants: Vec<Variant>,
}

// ─── Variants ───

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct VariantRow {
    pub id: String,
    pub product_id: String,
    pub sku: String,
    pub barcode: Option<String>,
    pub stock: i32,
    pub low_stock_at: i32,
    pub size: String,
    pub weight_kg: Option<f64>,
    pub price: i32,
    pub mrp: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Variant {
    pub id: String,
    pub sku: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub barcode: Option<String>,
    pub stock: i32,
    pub low_stock_at: i32,
    pub size: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight_kg: Option<f64>,
    pub price: i32,
    pub mrp: i32,
    pub in_stock: bool,
}

// ─── Reviews ───

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ReviewRow {
    pub id: String,
    pub product_id: String,
    pub author: String,
    pub pet_label: String,
    pub pet_species: String,
    pub rating: i16,
    pub title: String,
    pub body: String,
    pub date: String,
    pub helpful: i32,
    pub order_id: Option<String>,
    pub reply: Option<serde_json::Value>,
    pub media: Vec<String>,
    pub status: String,
    pub featured: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub id: String,
    pub product_id: String,
    pub author: String,
    pub pet_label: String,
    pub pet_species: String,
    pub rating: i16,
    pub title: String,
    pub body: String,
    pub date: String,
    pub helpful: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<String>,
    pub status: String,
    #[serde(default)]
    pub featured: bool,
}

// ─── Coupons ───

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct CouponRow {
    pub code: String,
    pub title: String,
    pub description: String,
    pub kind: String,
    pub value: i32,
    pub max_discount: Option<i32>,
    pub min_order: i32,
    pub first_order_only: bool,
    pub categories: Vec<String>,
    pub product_ids: Vec<String>,
    pub excludes_autoship: bool,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub usage_limit: Option<i32>,
    pub per_customer_limit: Option<i32>,
    pub used: i32,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Coupon {
    pub code: String,
    pub title: String,
    pub description: String,
    pub kind: String,
    pub value: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_discount: Option<i32>,
    pub min_order: i32,
    #[serde(default)]
    pub first_order_only: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub product_ids: Vec<String>,
    pub excludes_autoship_lines: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starts_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_limit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_customer_limit: Option<i32>,
    pub used: i32,
    pub active: bool,
}

// ─── Offers ───

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct OfferRow {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub line: String,
    pub image: String,
    pub badge: String,
    pub link: String,
    pub cta: String,
    pub coupon_code: Option<String>,
    pub species: String,
    pub food: bool,
    pub flash: bool,
    pub starts_at: String,
    pub ends_at: Option<String>,
    pub active: bool,
    pub in_loader: bool,
    pub poster_line: Option<String>,
    pub poster_big: Option<String>,
    pub poster_image: Option<String>,
}

// ─── Inventory transactions ───

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct InventoryTxnRow {
    pub id: String,
    pub variant_id: String,
    pub product_id: String,
    #[sqlx(rename = "type")]
    pub txn_type: String,
    pub qty: i32,
    pub before_qty: i32,
    pub after_qty: i32,
    pub reason: String,
    pub by_whom: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryTxn {
    pub id: String,
    pub at: String,
    pub product_id: String,
    pub variant_id: String,
    #[serde(rename = "type")]
    pub txn_type: String,
    pub qty: i32,
    pub before: i32,
    pub after: i32,
    pub reason: String,
    pub by: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdjustStockInput {
    /// Ignored: the variant's own product is recorded. Kept so older clients still parse.
    #[serde(default)]
    pub product_id: String,
    pub variant_id: String,
    /// The change (+ adds, - removes). Ignored when `counted` is given.
    #[serde(default)]
    pub qty: i32,
    /// A stock count: set the stock to exactly this.
    #[serde(default)]
    pub counted: Option<i32>,
    #[serde(rename = "type")]
    pub txn_type: String,
    pub reason: String,
}

// ─── Sample campaigns ───

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct SampleCampaignRow {
    pub id: String,
    pub name: String,
    pub product_id: String,
    pub size: String,
    pub starts_at: String,
    pub ends_at: String,
    pub max_claims: i32,
    pub claims: i32,
    pub per_household: i32,
    pub delivery_fee: i32,
    pub first_order_only: bool,
    pub active: bool,
}

// ─── Message templates ───

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct MessageTemplateRow {
    pub id: String,
    pub name: String,
    pub event: String,
    pub channel: String,
    pub body: String,
    pub approval: String,
    pub active: bool,
}

// ─── Care templates ───

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct CareTemplateRow {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub species: Vec<String>,
    pub first_due_weeks: i32,
    pub repeat_months: i32,
    pub remind_days_before: i32,
    pub channels: Vec<String>,
    pub message: String,
    pub active: bool,
}

// ─── Admin settings (key-value) ───

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct AdminSettingRow {
    pub key: String,
    pub value: serde_json::Value,
}

// ─── Conversions ───

impl From<VariantRow> for Variant {
    fn from(v: VariantRow) -> Self {
        Self {
            id: v.id,
            sku: v.sku,
            barcode: v.barcode,
            stock: v.stock,
            low_stock_at: v.low_stock_at,
            size: v.size,
            weight_kg: v.weight_kg,
            price: v.price,
            mrp: v.mrp,
            in_stock: v.stock > 0,
        }
    }
}

impl ProductRow {
    pub fn into_product(self, variants: Vec<Variant>, created_at: String) -> Product {
        Product {
            id: self.id,
            slug: self.slug,
            name: self.name,
            brand: self.brand,
            species: self.species,
            category: self.category_slug,
            subcategory: self.subcategory,
            life_stage: self.life_stage,
            breed_size: self.breed_size,
            diet: self.diet,
            grain_free: if self.grain_free { Some(true) } else { None },
            allergens: self.allergens,
            summary: self.summary,
            description: self.description,
            ingredients: self.ingredients,
            nutrition: self.nutrition,
            best_before: self.best_before,
            country_of_origin: self.country_of_origin,
            images: self.images,
            benefits: self.benefits,
            videos: self.videos,
            suitable_breeds: self.suitable_breeds,
            feeding_instructions: self.feeding_instructions,
            status: self.status,
            popularity: self.popularity,
            created_at,
            autoship_eligible: self.autoship_eligible,
            gst_rate_pct: self.gst_rate_pct,
            hsn: self.hsn,
            is_new: self.is_new,
            variants,
        }
    }
}

impl From<ReviewRow> for Review {
    fn from(r: ReviewRow) -> Self {
        Self {
            id: r.id,
            product_id: r.product_id,
            author: r.author,
            pet_label: r.pet_label,
            pet_species: r.pet_species,
            rating: r.rating,
            title: r.title,
            body: r.body,
            date: r.date,
            helpful: r.helpful,
            order_id: r.order_id,
            reply: r.reply,
            media: r.media,
            status: r.status,
            featured: r.featured,
        }
    }
}

impl From<CouponRow> for Coupon {
    fn from(c: CouponRow) -> Self {
        Self {
            code: c.code,
            title: c.title,
            description: c.description,
            kind: c.kind,
            value: c.value,
            max_discount: c.max_discount,
            min_order: c.min_order,
            first_order_only: c.first_order_only,
            categories: c.categories,
            product_ids: c.product_ids,
            excludes_autoship_lines: c.excludes_autoship,
            starts_at: c.starts_at,
            ends_at: c.ends_at,
            usage_limit: c.usage_limit,
            per_customer_limit: c.per_customer_limit,
            used: c.used,
            active: c.active,
        }
    }
}

fn default_tone() -> String {
    "fog".into()
}

fn default_true() -> bool {
    true
}

fn is_false(b: &bool) -> bool {
    !b
}
