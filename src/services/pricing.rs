//! The one price calculator. A line-by-line port of the website's `computePricing`
//! (customer-website/src/lib/pricing.ts) so the server and the browser agree to the rupee, but the
//! server's answer is the one that counts: the browser's totals are display-only.
//!
//! Catalogue prices are whole rupees and every rule (Autoship 10 %, coupon %, fees) rounds to whole
//! rupees exactly like the website, then everything is turned into paise for storage and the ledger.
//! GST is *included* in prices; each line's GST and share of the coupon discount are worked out in
//! paise so invoices and GST reports add up exactly.

use serde::Serialize;

pub const FREE_DELIVERY_AT: i64 = 999;
pub const STANDARD_DELIVERY_FEE: i64 = 49;
pub const EXPRESS_DELIVERY_FEE: i64 = 99;
pub const COD_FEE: i64 = 29;
pub const COD_MAX_ORDER: i64 = 5000;
pub const AUTOSHIP_DISCOUNT_PCT: i64 = 10;
/// GST on delivery and COD fees (services, 18 %). Confirm with the CA.
pub const FEE_GST_RATE_PCT: i64 = 18;

pub const fn paise(rupees: i64) -> i64 {
    rupees * 100
}

/// `round(value * pct / 100)` for non-negative whole numbers, rounding halves up like JavaScript's Math.round.
fn pct_of(value: i64, pct: i64) -> i64 {
    (value * pct + 50) / 100
}

/// GST hidden inside a GST-inclusive amount: `amount × rate / (100 + rate)`, rounded to the nearest paisa.
pub fn gst_inside(amount_paise: i64, rate_pct: i64) -> i64 {
    if rate_pct <= 0 {
        return 0;
    }
    let denom = 100 + rate_pct;
    (amount_paise * rate_pct + denom / 2) / denom
}

pub fn autoship_price(price: i64) -> i64 {
    pct_of(price, 100 - AUTOSHIP_DISCOUNT_PCT)
}

/// One cart line with the catalogue facts it needs (read from the database, never from the browser).
#[derive(Debug, Clone)]
pub struct LineInput {
    pub product_id: String,
    pub variant_id: String,
    pub category: String,
    pub name: String,
    pub size: String,
    pub hsn: String,
    pub gst_rate_pct: i64,
    /// Rupees, GST included.
    pub price: i64,
    pub mrp: i64,
    pub qty: i64,
    pub autoship: bool,
    pub frequency_days: Option<i32>,
}

/// The coupon as stored in the `coupons` table.
#[derive(Debug, Clone)]
pub struct CouponRule {
    pub code: String,
    pub kind: String,
    pub value: i64,
    pub max_discount: Option<i64>,
    pub min_order: i64,
    pub first_order_only: bool,
    pub categories: Vec<String>,
    pub product_ids: Vec<String>,
    pub excludes_autoship: bool,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub usage_limit: Option<i64>,
    pub per_customer_limit: Option<i64>,
    pub used: i64,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryKind {
    Standard,
    Express,
}

#[derive(Debug, Clone)]
pub struct PricingInput<'a> {
    pub lines: Vec<LineInput>,
    pub coupon_code: Option<&'a str>,
    /// `None` when the code isn't in the database.
    pub coupon: Option<CouponRule>,
    pub is_first_order: bool,
    pub coupon_uses_by_customer: i64,
    pub delivery: DeliveryKind,
    pub cash_on_delivery: bool,
    /// Rupees; 0 = not chosen.
    pub donation: i64,
    /// Today as YYYY-MM-DD (India time), for coupon start and end dates.
    pub today: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PricedLine {
    pub product_id: String,
    pub variant_id: String,
    pub name: String,
    pub size: String,
    pub hsn: String,
    pub gst_rate_pct: i64,
    pub qty: i64,
    pub unit_price: i64,
    pub mrp: i64,
    pub line_total: i64,
    pub autoship: bool,
    pub frequency_days: Option<i32>,
    /// This line's share of the coupon discount, in paise.
    pub discount_paise: i64,
    /// GST inside what the customer pays for this line (after its discount share), in paise.
    pub gst_paise: i64,
    #[serde(skip)]
    category: String,
    #[serde(skip)]
    autoship_saving: i64,
}

impl PricedLine {
    /// What the customer pays for this line after its share of the coupon, in paise.
    pub fn net_paise(&self) -> i64 {
        paise(self.line_total) - self.discount_paise
    }
}

/// All amounts in whole rupees except where a field says paise.
#[derive(Debug, Clone, Serialize)]
pub struct Quote {
    pub lines: Vec<PricedLine>,
    pub mrp_total: i64,
    pub items_total: i64,
    pub product_discount: i64,
    pub autoship_savings: i64,
    pub coupon_code: Option<String>,
    pub coupon_discount: i64,
    pub coupon_error: Option<String>,
    pub delivery: i64,
    pub cod_fee: i64,
    pub cod_allowed: bool,
    pub donation: i64,
    pub wallet_used: i64,
    pub total: i64,
    pub gst_paise: i64,
    pub item_count: i64,
}

enum CouponCheck {
    Ok {
        discount: i64,
        free_shipping: bool,
        /// Indexes (into the priced lines) of the lines the coupon applies to. The discount, and so
        /// the GST inside it, is shared over exactly these lines.
        eligible: Vec<usize>,
    },
    No(String),
}

fn check_coupon(input: &PricingInput, lines: &[PricedLine]) -> CouponCheck {
    let Some(c) = input.coupon.as_ref().filter(|c| c.active) else {
        return CouponCheck::No(
            "We don’t recognise that code. Check the spelling and try again.".into(),
        );
    };
    let today = input.today.as_str();
    if let Some(start) = c.starts_at.as_deref().filter(|s| today < *s) {
        return CouponCheck::No(format!("{} starts on {start}.", c.code));
    }
    if c.ends_at.as_deref().is_some_and(|e| today > e) {
        return CouponCheck::No(format!("{} has ended.", c.code));
    }
    if c.usage_limit.is_some_and(|limit| c.used >= limit) {
        return CouponCheck::No(format!("{} has been fully used up.", c.code));
    }
    if c.per_customer_limit
        .is_some_and(|limit| input.coupon_uses_by_customer >= limit)
    {
        return CouponCheck::No(format!("You’ve already used {}.", c.code));
    }
    if c.first_order_only && !input.is_first_order {
        return CouponCheck::No(format!("{} is for first orders only.", c.code));
    }
    let eligible_idx: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            (c.categories.is_empty() || c.categories.contains(&l.category))
                && (c.product_ids.is_empty() || c.product_ids.contains(&l.product_id))
                && !(c.excludes_autoship && l.autoship)
        })
        .map(|(i, _)| i)
        .collect();
    let eligible: Vec<&PricedLine> = eligible_idx.iter().map(|&i| &lines[i]).collect();
    let eligible_total: i64 = eligible.iter().map(|l| l.line_total).sum();
    if eligible_total == 0 {
        let selected = !c.categories.is_empty() || !c.product_ids.is_empty();
        return CouponCheck::No(if selected {
            format!(
                "{} works on selected items only{}.",
                c.code,
                if c.excludes_autoship {
                    ", and not on Autoship items"
                } else {
                    ""
                }
            )
        } else {
            format!(
                "{} doesn’t apply to Autoship items, which already get {AUTOSHIP_DISCOUNT_PCT}% off.",
                c.code
            )
        });
    }
    if eligible_total < c.min_order {
        return CouponCheck::No(format!(
            "Add ₹{} more of eligible items to use {}.",
            c.min_order - eligible_total,
            c.code
        ));
    }
    match c.kind.as_str() {
        "free-shipping" => CouponCheck::Ok {
            discount: 0,
            free_shipping: true,
            eligible: eligible_idx,
        },
        "bogo" => {
            // Every second unit of an eligible item is free.
            let free: i64 = eligible.iter().map(|l| (l.qty / 2) * l.unit_price).sum();
            if free == 0 {
                return CouponCheck::No(format!(
                    "Add a second pack of the offer item to use {}.",
                    c.code
                ));
            }
            CouponCheck::Ok {
                discount: free,
                free_shipping: false,
                eligible: eligible_idx,
            }
        }
        kind => {
            let raw = if kind == "percent" {
                pct_of(eligible_total, c.value)
            } else {
                c.value
            };
            let discount = raw
                .min(c.max_discount.unwrap_or(raw))
                .min(eligible_total)
                .max(0);
            CouponCheck::Ok {
                discount,
                free_shipping: false,
                eligible: eligible_idx,
            }
        }
    }
}

/// Spread `total_paise` over the `only` lines in proportion to their totals; the last of them takes
/// the rounding remainder so the parts always add back to exactly `total_paise`. Other lines keep
/// no discount, so each line's GST is worked out on what was really charged for it.
fn spread(total_paise: i64, lines: &mut [PricedLine], only: &[usize]) {
    let base: i64 = only.iter().map(|&i| paise(lines[i].line_total)).sum();
    if base == 0 || total_paise == 0 {
        return;
    }
    let mut left = total_paise;
    for (n, &i) in only.iter().enumerate() {
        let line_paise = paise(lines[i].line_total);
        let share = if n + 1 == only.len() {
            left
        } else {
            total_paise * line_paise / base
        };
        lines[i].discount_paise = share.min(line_paise);
        left -= lines[i].discount_paise;
    }
}

pub fn compute(input: &PricingInput) -> Quote {
    let mut lines: Vec<PricedLine> = input
        .lines
        .iter()
        .filter(|l| l.qty > 0)
        .map(|l| {
            let unit_price = if l.autoship {
                autoship_price(l.price)
            } else {
                l.price
            };
            PricedLine {
                product_id: l.product_id.clone(),
                variant_id: l.variant_id.clone(),
                name: l.name.clone(),
                size: l.size.clone(),
                hsn: l.hsn.clone(),
                gst_rate_pct: l.gst_rate_pct,
                qty: l.qty,
                unit_price,
                mrp: l.mrp,
                line_total: unit_price * l.qty,
                autoship: l.autoship,
                frequency_days: l.frequency_days,
                discount_paise: 0,
                gst_paise: 0,
                category: l.category.clone(),
                autoship_saving: (l.price - unit_price) * l.qty,
            }
        })
        .collect();

    let items_total: i64 = lines.iter().map(|l| l.line_total).sum();
    let mrp_total: i64 = lines.iter().map(|l| l.mrp * l.qty).sum();
    let autoship_savings: i64 = lines.iter().map(|l| l.autoship_saving).sum();

    let (mut coupon_discount, mut free_shipping, mut coupon_code, mut coupon_error) =
        (0, false, None, None);
    let mut eligible_lines: Vec<usize> = Vec::new();
    if let Some(code) = input.coupon_code.map(str::trim).filter(|c| !c.is_empty())
        && !lines.is_empty()
    {
        match check_coupon(input, &lines) {
            CouponCheck::Ok {
                discount,
                free_shipping: f,
                eligible,
            } => {
                coupon_discount = discount;
                free_shipping = f;
                eligible_lines = eligible;
                coupon_code = Some(code.to_uppercase());
            }
            CouponCheck::No(reason) => coupon_error = Some(reason),
        }
    }

    let after_discounts = items_total - coupon_discount;
    let to_free_delivery = (FREE_DELIVERY_AT - after_discounts).max(0);
    let standard_fee = if lines.is_empty() || to_free_delivery == 0 || free_shipping {
        0
    } else {
        STANDARD_DELIVERY_FEE
    };
    let delivery = match input.delivery {
        DeliveryKind::Express => EXPRESS_DELIVERY_FEE,
        DeliveryKind::Standard => standard_fee,
    };
    let cod_allowed = after_discounts + delivery <= COD_MAX_ORDER;
    let cod_fee = if input.cash_on_delivery { COD_FEE } else { 0 };
    let donation = if lines.is_empty() {
        0
    } else {
        input.donation.max(0)
    };
    let total = (after_discounts + delivery + cod_fee + donation).max(0);

    spread(paise(coupon_discount), &mut lines, &eligible_lines);
    for l in &mut lines {
        l.gst_paise = gst_inside(l.net_paise(), l.gst_rate_pct);
    }
    // Fees are taxed one by one, exactly as the ledger posts them.
    let gst_paise = lines.iter().map(|l| l.gst_paise).sum::<i64>()
        + gst_inside(paise(delivery), FEE_GST_RATE_PCT)
        + gst_inside(paise(cod_fee), FEE_GST_RATE_PCT);

    Quote {
        item_count: lines.iter().map(|l| l.qty).sum(),
        lines,
        mrp_total,
        items_total,
        product_discount: mrp_total - items_total - autoship_savings,
        autoship_savings,
        coupon_code,
        coupon_discount,
        coupon_error,
        delivery,
        cod_fee,
        cod_allowed,
        donation,
        // The wallet moves to the server with Paw Points; until then nothing is taken from it here.
        wallet_used: 0,
        total,
        gst_paise,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(price: i64, mrp: i64, qty: i64) -> LineInput {
        LineInput {
            product_id: "salmon-oats".into(),
            variant_id: "salmon-oats-3".into(),
            category: "dry-food".into(),
            name: "Salmon & Oats Adult".into(),
            size: "3 kg".into(),
            hsn: "2309".into(),
            gst_rate_pct: 18,
            price,
            mrp,
            qty,
            autoship: false,
            frequency_days: None,
        }
    }

    fn coupon(kind: &str, value: i64) -> CouponRule {
        CouponRule {
            code: "TEST".into(),
            kind: kind.into(),
            value,
            max_discount: None,
            min_order: 0,
            first_order_only: false,
            categories: vec![],
            product_ids: vec![],
            excludes_autoship: false,
            starts_at: None,
            ends_at: None,
            usage_limit: None,
            per_customer_limit: None,
            used: 0,
            active: true,
        }
    }

    fn input(lines: Vec<LineInput>) -> PricingInput<'static> {
        PricingInput {
            lines,
            coupon_code: None,
            coupon: None,
            is_first_order: false,
            coupon_uses_by_customer: 0,
            delivery: DeliveryKind::Standard,
            cash_on_delivery: false,
            donation: 0,
            today: "2026-10-05".into(),
        }
    }

    #[test]
    fn small_order_pays_standard_delivery_and_big_one_is_free() {
        let q = compute(&input(vec![line(949, 1099, 1)]));
        assert_eq!((q.items_total, q.delivery, q.total), (949, 49, 998));
        let q = compute(&input(vec![line(949, 1099, 2)]));
        assert_eq!((q.delivery, q.total), (0, 1898));
    }

    #[test]
    fn autoship_takes_ten_percent_rounded_like_the_website() {
        let mut l = line(949, 1099, 1);
        l.autoship = true;
        let q = compute(&input(vec![l]));
        // Math.round(949 * 0.9) = 854
        assert_eq!((q.lines[0].unit_price, q.autoship_savings), (854, 95));
    }

    #[test]
    fn percent_coupon_is_capped_by_max_discount() {
        let mut i = input(vec![line(2149, 2499, 1)]);
        let mut c = coupon("percent", 10);
        c.max_discount = Some(100);
        i.coupon_code = Some("test");
        i.coupon = Some(c);
        let q = compute(&i);
        assert_eq!(
            (q.coupon_discount, q.coupon_code.as_deref()),
            (100, Some("TEST"))
        );
        assert_eq!(q.total, 2049);
    }

    #[test]
    fn coupon_reasons_are_explained() {
        let mut i = input(vec![line(300, 350, 1)]);
        let mut c = coupon("flat", 50);
        c.min_order = 999;
        i.coupon_code = Some("TEST");
        i.coupon = Some(c);
        let q = compute(&i);
        assert_eq!(q.coupon_discount, 0);
        assert_eq!(
            q.coupon_error.as_deref(),
            Some("Add ₹699 more of eligible items to use TEST.")
        );

        let mut i = input(vec![line(300, 350, 1)]);
        i.coupon_code = Some("NOPE");
        assert!(
            compute(&i)
                .coupon_error
                .unwrap()
                .contains("don’t recognise")
        );

        let mut i = input(vec![line(300, 350, 1)]);
        let mut c = coupon("flat", 50);
        c.first_order_only = true;
        i.coupon_code = Some("TEST");
        i.coupon = Some(c);
        assert!(
            compute(&i)
                .coupon_error
                .unwrap()
                .contains("first orders only")
        );
    }

    #[test]
    fn bogo_makes_every_second_unit_free() {
        let mut i = input(vec![line(199, 249, 3)]);
        i.coupon_code = Some("TEST");
        i.coupon = Some(coupon("bogo", 0));
        assert_eq!(compute(&i).coupon_discount, 199);
    }

    #[test]
    fn express_cod_and_donation_add_up() {
        let mut i = input(vec![line(949, 1099, 1)]);
        i.delivery = DeliveryKind::Express;
        i.cash_on_delivery = true;
        i.donation = 5;
        let q = compute(&i);
        assert_eq!(
            (q.delivery, q.cod_fee, q.donation, q.total),
            (99, 29, 5, 1082)
        );
        assert!(q.cod_allowed);
    }

    #[test]
    fn cod_not_allowed_over_the_limit() {
        assert!(!compute(&input(vec![line(5999, 7299, 1)])).cod_allowed);
    }

    #[test]
    fn discount_shares_add_back_to_the_whole_discount() {
        let mut i = input(vec![
            line(333, 400, 1),
            line(667, 700, 1),
            line(101, 150, 1),
        ]);
        i.coupon_code = Some("TEST");
        i.coupon = Some(coupon("flat", 100));
        let q = compute(&i);
        let shares: i64 = q.lines.iter().map(|l| l.discount_paise).sum();
        assert_eq!(shares, paise(100));
        assert!(
            q.lines
                .iter()
                .all(|l| l.discount_paise <= paise(l.line_total))
        );
    }

    #[test]
    fn gst_inside_inclusive_prices() {
        // ₹118 at 18 % holds ₹18 of GST.
        assert_eq!(gst_inside(11_800, 18), 1_800);
        assert_eq!(gst_inside(10_000, 0), 0);
        // ₹949 at 18 %: 94900 × 18 / 118 = 14476.27… → 14476
        assert_eq!(gst_inside(94_900, 18), 14_476);
    }

    #[test]
    fn coupon_discount_only_touches_the_lines_it_applies_to() {
        let mut food = line(1000, 1200, 1);
        food.category = "dry-food".into();
        let mut toy = line(1000, 1200, 1);
        toy.product_id = "toy".into();
        toy.variant_id = "toy-1".into();
        toy.category = "toys".into();
        toy.gst_rate_pct = 5;
        let mut i = input(vec![food, toy]);
        let mut c = coupon("percent", 10);
        c.categories = vec!["dry-food".into()];
        i.coupon_code = Some("TEST");
        i.coupon = Some(c);
        let q = compute(&i);
        assert_eq!(q.coupon_discount, 100);
        let food_line = q.lines.iter().find(|l| l.category == "dry-food").unwrap();
        let toy_line = q.lines.iter().find(|l| l.category == "toys").unwrap();
        assert_eq!(food_line.discount_paise, paise(100));
        assert_eq!(toy_line.discount_paise, 0);
        // GST on what was actually charged: 900 at 18 % and 1000 at 5 %.
        assert_eq!(food_line.gst_paise, gst_inside(90_000, 18));
        assert_eq!(toy_line.gst_paise, gst_inside(100_000, 5));
    }

    #[test]
    fn empty_cart_costs_nothing() {
        let mut i = input(vec![]);
        i.donation = 5;
        let q = compute(&i);
        assert_eq!((q.total, q.delivery, q.donation), (0, 0, 0));
    }
}
