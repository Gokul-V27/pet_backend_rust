-- Seed data for Wagwell catalogue: categories, sample campaign, coupons,
-- message templates, care schedules, and products with variants.
-- All queries use ON CONFLICT DO NOTHING to remain idempotent.

------- Categories -------
INSERT INTO categories (slug, label, blurb, photo, tone, parent_slug, active, sort) VALUES
('dry-food', 'Dry food', 'Complete daily meals', '/images/categories/dry-food.webp', 'fog', NULL, TRUE, 1),
('wet-food', 'Wet food', 'Gravies and loaves', '/images/categories/wet-food.webp', 'sky', NULL, TRUE, 2),
('treats', 'Treats', 'Rewards and chews', '/images/categories/treats.webp', 'pink', NULL, TRUE, 3),
('dental-chews', 'Dental chews', 'Daily chewing', '/images/categories/dental-chews.webp', 'pink', 'treats', TRUE, 4),
('health', 'Supplements', 'Oils and add-ons', '/images/categories/health.webp', 'sky', NULL, TRUE, 5),
('toys', 'Toys', 'Chew, tug, sniff', '/images/categories/toys.webp', 'mint', NULL, TRUE, 6),
('accessories', 'Accessories', 'Harnesses and bowls', '/images/categories/accessories.webp', 'sky', NULL, TRUE, 7),
('grooming', 'Grooming', 'Bath and coat care', '/images/categories/grooming.webp', 'pink', NULL, TRUE, 8)
ON CONFLICT (slug) DO NOTHING;

------- Sample Campaign -------
INSERT INTO sample_campaigns (id, name, product_id, size, starts_at, ends_at, max_claims, claims, per_household, delivery_fee, first_order_only, active) VALUES
('first-bowl', 'First bowl — free 200 g sample', 'lamb-rice-adult', '200 g', '2026-09-01', '2026-12-31', 1000, 0, 1, 0, TRUE, TRUE)
ON CONFLICT (id) DO NOTHING;

------- Coupons -------
INSERT INTO coupons (code, title, description, kind, value, max_discount, min_order, first_order_only, categories, product_ids, excludes_autoship, starts_at, ends_at, usage_limit, per_customer_limit, used, active) VALUES
('FIRSTBOWL', '10% off your first order', 'For new customers. Minimum order ₹999, up to ₹300 off.', 'percent', 10, 300, 999, TRUE, '{}', '{}', TRUE, NULL, NULL, NULL, 1, 0, TRUE),
('TREATS15', '15% off treats', 'On treats and chews when your treats add up to ₹499 or more.', 'percent', 15, NULL, 499, FALSE, '{treats}', '{}', TRUE, NULL, NULL, NULL, NULL, 0, TRUE),
('FLAT100', '₹100 off orders over ₹1,500', 'Flat ₹100 off. Valid till 31 Dec 2026, 500 uses.', 'flat', 100, NULL, 1500, FALSE, '{}', '{}', TRUE, NULL, '2026-12-31', 500, NULL, 0, TRUE),
('FREESHIP', 'Free delivery on any order', 'Free standard delivery, no minimum. One use per customer.', 'free-shipping', 0, NULL, 0, FALSE, '{}', '{}', FALSE, NULL, NULL, NULL, 1, 0, TRUE)
ON CONFLICT (code) DO NOTHING;

------- Message Templates -------
INSERT INTO message_templates (id, name, event, channel, body, approval, active) VALUES
('t-placed', 'order_placed', 'Order placed', 'whatsapp', 'Hi {{name}}, thanks for your order {{order}} of {{amount}}. We’ll deliver on {{date}}.', 'approved', TRUE),
('t-shipped', 'order_shipped', 'Order shipped', 'whatsapp', 'Your order {{order}} is on its way. Track it here: {{link}}', 'approved', TRUE),
('t-delivered', 'order_delivered', 'Order delivered', 'whatsapp', 'Your order {{order}} was delivered. Hope {{pet}} enjoys it!', 'approved', TRUE),
('t-payfail', 'payment_failed', 'Payment failed', 'whatsapp', 'Your payment for {{amount}} didn’t go through. No money was taken. Try again: {{link}}', 'approved', TRUE),
('t-autoship', 'autoship_reminder', 'Autoship in 3 days', 'whatsapp', 'Your Autoship for {{pet}} ships on {{date}}. Skip or change it: {{link}}', 'approved', TRUE),
('t-vaccine', 'vaccine_reminder', 'Vaccination due', 'whatsapp', '{{pet}}’s {{vaccine}} is due on {{date}}. Please book with your vet.', 'pending', TRUE),
('t-invoice', 'Invoice email', 'Order placed', 'email', 'Hi {{name}}, your invoice for order {{order}} is attached.', 'approved', TRUE)
ON CONFLICT (id) DO NOTHING;

------- Care Templates -------
INSERT INTO care_templates (id, kind, name, species, first_due_weeks, repeat_months, remind_days_before, channels, message, active) VALUES
('rabies', 'vaccine', 'Rabies vaccine', '{dog,cat}', 12, 12, 14, '{whatsapp,website}', 'Rabies vaccine due for {{petName}}', TRUE),
('dhpp', 'vaccine', 'DHPP / 7-in-1', '{dog}', 8, 12, 14, '{whatsapp,website}', '7-in-1 booster due for {{petName}}', TRUE),
('deworming', 'deworming', 'Routine deworming', '{dog,cat}', 4, 3, 7, '{whatsapp,website}', 'Deworming due for {{petName}}', TRUE)
ON CONFLICT (id) DO NOTHING;

------- Initial Products -------
INSERT INTO products (id, slug, name, brand, species, category_slug, subcategory, life_stage, breed_size, diet, grain_free, allergens, summary, description, ingredients, nutrition, best_before, country_of_origin, images, benefits, videos, suitable_breeds, feeding_instructions, status, popularity, autoship_eligible, gst_rate_pct, hsn, is_new) VALUES
('lamb-rice-adult', 'lamb-brown-rice-adult', 'Lamb & Brown Rice Adult', 'Wagwell', '{dog}', 'dry-food', NULL, 'adult', 'all', 'non-veg', FALSE, '{lamb}',
 'Everyday dry food with lamb as the only animal protein. No chicken.',
 'A complete dry food for adult dogs, made with lamb, brown rice, pumpkin and coconut oil. Lamb is the only animal protein, so it suits dogs that need to avoid chicken.',
 '{Lamb (28%),Brown rice,Oats,Pumpkin,Coconut oil,Vitamins and minerals}',
 '{"proteinPct": 26, "fatPct": 14, "fibrePct": 4, "moisturePct": 10, "kcalPerKg": 3650}',
 'Jun 2027', 'India', '{photo:pLamb,photo:pLambLife,photo:indieDog}',
 '{Chicken-free,"Baked in small batches",Digestion,"Single protein"}', '{}', '{Indie,Labrador,"Golden Retriever",Beagle}',
 '200g - 350g daily depending on weight and activity level.',
 'active', 120, TRUE, 18, '2309', FALSE),

('salmon-oats', 'salmon-oats-adult', 'Salmon & Oats Adult', 'Wagwell', '{dog}', 'dry-food', NULL, 'adult', 'all', 'non-veg', FALSE, '{fish}',
 'Dry food with salmon and oats. No chicken, beef or corn.',
 'A complete dry food for adult dogs made with salmon, rolled oats and flaxseed. It contains no chicken, beef or corn.',
 '{Salmon (26%),Rolled oats,Brown rice,Flaxseed,Sunflower oil,Vitamins and minerals}',
 '{"proteinPct": 27, "fatPct": 15, "fibrePct": 3.8, "moisturePct": 9.5, "kcalPerKg": 3690}',
 'Jul 2027', 'India', '{photo:pSalmon,photo:pSalmonLife,photo:kibbleBowl}',
 '{Skin-and-coat,"Omega-3 rich",Hypoallergenic}', '{}', '{Indie,Shih-Tzu,"German Shepherd",Pug}',
 '180g - 320g daily divided into two meals.',
 'active', 95, TRUE, 18, '2309', FALSE),

('puppy-fish', 'fish-sweet-potato-puppy', 'Fish & Sweet Potato Puppy', 'Wagwell', '{dog}', 'dry-food', NULL, 'puppy', 'medium', 'non-veg', TRUE, '{fish}',
 'Grain-free dry food sized for puppies up to 12 months.',
 'A complete grain-free food for puppies from 8 weeks to 12 months, made with ocean fish and sweet potato. Small kibble that is easy for puppies to chew.',
 '{Ocean fish (30%),Sweet potato,Peas,Fish oil,Pumpkin,Vitamins and minerals}',
 '{"proteinPct": 30, "fatPct": 16, "fibrePct": 3.5, "moisturePct": 10, "kcalPerKg": 3780}',
 'May 2027', 'India', '{photo:pPuppy,photo:pPuppyLife,photo:puppyTilt}',
 '{Grain-free,Growth,"Small kibble",Brain-development}', '{}', '{"All puppies under 12 months"}',
 'Feed 3 times daily up to 6 months, twice daily afterwards.',
 'active', 75, TRUE, 18, '2309', TRUE),

('cat-fish-adult', 'ocean-fish-adult-cat', 'Ocean Fish Adult Cat', 'Wagwell', '{cat}', 'dry-food', NULL, 'adult', 'all', 'non-veg', FALSE, '{fish}',
 'Complete dry food for adult cats, with added taurine.',
 'A complete dry food for adult cats made with ocean fish and rice, with added taurine. Keep fresh water available at all times.',
 '{Ocean fish (32%),Rice,Fish oil,Taurine,Vitamins and minerals}',
 '{"proteinPct": 32, "fatPct": 12, "fibrePct": 3, "moisturePct": 10, "kcalPerKg": 3800}',
 'Jun 2027', 'India', '{photo:pCat,photo:pCatLife,photo:cat}',
 '{Taurine-enriched,Hairball-control,Urinary-tract}', '{}', '{"All adult cats"}',
 '50g - 80g daily divided into multiple small portions.',
 'active', 80, TRUE, 18, '2309', TRUE),

('veg-dental-chews', 'veg-dental-chews', 'Veg Dental Chews', 'Paws & Polish', '{dog}', 'treats', 'dental-chews', 'all', 'all', 'veg', TRUE, '{}',
 'Ridged vegetarian chews for daily chewing. About 85 kcal each.',
 'Plant-based chews with ridges that give your dog something to work on, flavoured with mint and parsley. Give one a day as a treat.',
 '{Pea protein,Sweet potato,Vegetable glycerin,Mint,Parsley,Calcium carbonate}',
 '{"proteinPct": 14, "fatPct": 3.5, "fibrePct": 6, "moisturePct": 14, "kcalPerKg": 2900}',
 'Aug 2027', 'India', '{photo:pChews,photo:chews}',
 '{Plaque-control,"Fresh breath",Vegetarian}', '{}', '{"All dogs over 6 months"}',
 '1 chew per day after meals.',
 'active', 150, TRUE, 18, '2309', FALSE)
ON CONFLICT (id) DO NOTHING;

------- Product Variants -------
INSERT INTO variants (id, product_id, sku, barcode, stock, low_stock_at, size, weight_kg, price, mrp) VALUES
('lra-1-2', 'lamb-rice-adult', 'WG-LRA-1-2', '890123456701', 42, 10, '1.2 kg', 1.2, 949, 1099),
('lra-3', 'lamb-rice-adult', 'WG-LRA-3', '890123456702', 120, 10, '3 kg', 3.0, 2149, 2499),
('lra-10', 'lamb-rice-adult', 'WG-LRA-10', '890123456703', 18, 5, '10 kg', 10.0, 5999, 7299),
('so-1-2', 'salmon-oats', 'WG-SO-1-2', '890123456704', 64, 10, '1.2 kg', 1.2, 1049, 1199),
('so-3', 'salmon-oats', 'WG-SO-3', '890123456705', 30, 10, '3 kg', 3.0, 2390, 2799),
('so-10', 'salmon-oats', 'WG-SO-10', '890123456706', 7, 5, '10 kg', 10.0, 6599, 7999),
('pf-1-2', 'puppy-fish', 'WG-PF-1-2', '890123456707', 88, 10, '1.2 kg', 1.2, 999, 1199),
('pf-3', 'puppy-fish', 'WG-PF-3', '890123456708', 25, 10, '3 kg', 3.0, 2299, 2699),
('cfa-1-2', 'cat-fish-adult', 'WG-CFA-1-2', '890123456709', 55, 10, '1.2 kg', 1.2, 899, 999),
('cfa-3', 'cat-fish-adult', 'WG-CFA-3', '890123456710', 40, 10, '3 kg', 3.0, 2099, 2399),
('vdc-14', 'veg-dental-chews', 'WG-VDC-14', '890123456711', 60, 10, '14 chews (210 g)', 0.21, 449, 499),
('vdc-28', 'veg-dental-chews', 'WG-VDC-28', '890123456712', 33, 10, '28 chews (420 g)', 0.42, 849, 999)
ON CONFLICT (id) DO NOTHING;

------- Initial Offers -------
INSERT INTO offers (id, kind, title, line, image, badge, link, cta, coupon_code, species, food, flash, active) VALUES
('offer-autoship', 'discount', 'Save 10% on Every Autoship', 'Scheduled deliveries on your terms. Skip or cancel anytime.', '/images/offers/autoship.webp', '10% OFF', '/autoship', 'Set up Autoship', NULL, 'all', TRUE, FALSE, TRUE),
('offer-sample', 'new', 'Try Our Lamb & Rice Recipe Free', 'Get a 200 g starter bowl delivered to your doorstep in Chennai.', '/images/offers/sample.webp', 'FREE SAMPLE', '/samples', 'Claim free bowl', 'FIRSTBOWL', 'dog', TRUE, FALSE, TRUE)
ON CONFLICT (id) DO NOTHING;
