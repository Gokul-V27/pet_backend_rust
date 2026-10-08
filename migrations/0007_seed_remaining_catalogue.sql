-- Seed the rest of the catalogue the website sells: the 9 products (with all 18 variants)
-- and 3 coupons that 0004_seed_catalogue.sql did not include, so orders for them are no longer
-- refused as 'no longer sold' and FLASH20 / BISCUITBOGO / PUPPYSTART are recognised.
-- Values come from customer-website/src/data/{catalog,coupons,benefits}.ts. Every insert uses
-- ON CONFLICT DO NOTHING, so it is safe to run when some of these rows already exist.
--
-- Placeholders to replace through the admin site before launch: stock levels are the website's demo
-- numbers, and barcodes continue the 0004 sequence (890123456713 onward). They are not real GTINs.

------- Remaining Products -------
INSERT INTO products (id, slug, name, brand, species, category_slug, subcategory, life_stage, breed_size, diet, grain_free, allergens, summary, description, ingredients, nutrition, best_before, country_of_origin, images, benefits, videos, suitable_breeds, feeding_instructions, status, popularity, autoship_eligible, gst_rate_pct, hsn, is_new) VALUES
-- Beef Senior
('senior-beef', 'beef-senior', 'Beef Senior', 'Wagwell', '{dog}', 'dry-food', NULL, 'senior', 'large', 'non-veg', FALSE, '{beef}',
 'Lower-calorie dry food for dogs aged 7 and over, with added glucosamine.',
 'A complete dry food for older dogs, with slightly fewer calories than our adult recipes and added glucosamine and chondroitin. If your senior dog has joint pain or a medical condition, please check with your vet before changing food.',
 '{"Beef (24%)","Brown rice",Barley,Carrot,Glucosamine,Chondroitin,"Vitamins and minerals"}',
 '{"proteinPct":24,"fatPct":12,"fibrePct":5,"moisturePct":10,"kcalPerKg":3450}'::jsonb,
 'Apr 2027', 'India', '{photo:pSenior,photo:pSeniorLife}',
 '{joints,weight,digestion}', '{}', '{}',
 NULL,
 'active', 0, TRUE, 18, '2309', FALSE),

-- Turmeric & Parsley Biscuits
('turmeric-biscuits', 'turmeric-parsley-biscuits', 'Turmeric & Parsley Biscuits', 'Paws & Polish', '{dog}', 'treats', NULL, 'all', 'all', 'veg', FALSE, '{wheat}',
 'Oven-baked vegetarian biscuits for training and rewards.',
 'Crunchy oven-baked biscuits made with whole wheat flour, turmeric and parsley. Break into pieces for training. Treats should be no more than a tenth of what your dog eats in a day.',
 '{"Whole wheat flour","Rice flour",Turmeric,Parsley,"Coconut oil","Black pepper"}',
 '{"proteinPct":16,"fatPct":7,"fibrePct":4.5,"moisturePct":8,"kcalPerKg":3300}'::jsonb,
 'Sep 2027', 'India', '{photo:pBiscuits,photo:treats}',
 '{joints,immunity}', '{}', '{}',
 NULL,
 'active', 0, TRUE, 18, '1905', FALSE),

-- Salmon Oil Pump
('salmon-oil', 'salmon-oil', 'Salmon Oil Pump', 'Wagwell', '{dog,cat}', 'health', NULL, 'all', 'all', 'non-veg', FALSE, '{fish}',
 'Salmon oil to pour over food. A source of omega-3 fatty acids.',
 'Cold-pressed salmon oil in a pump bottle. Add it to your pet’s food as a source of omega-3 fatty acids. Start with a small amount and follow the pump guide on the label. Keep in a cool place and use within 3 months of opening.',
 '{"Salmon oil","Mixed tocopherols (natural preservative)"}',
 NULL,
 'Mar 2027', 'Norway', '{photo:pOil}',
 '{coat,skin,joints}', '{}', '{}',
 NULL,
 'active', 0, TRUE, 18, '1504', FALSE),

-- Natural Rubber Chew Bone
('rubber-bone', 'rubber-chew-bone', 'Natural Rubber Chew Bone', 'Barkplay', '{dog}', 'toys', NULL, 'all', 'all', NULL, FALSE, '{}',
 'A sturdy rubber chew with a hollow centre you can fill with treats.',
 'A chew toy made from natural rubber, with grooves and a hollow centre that holds a little peanut butter or softened food. Supervise your dog, and replace the toy once pieces start to come off.',
 '{}',
 NULL,
 NULL, 'India', '{photo:pBone,photo:rubberBone}',
 '{}', '{}', '{}',
 NULL,
 'active', 0, FALSE, 18, '4016', FALSE),

-- Snuffle Mat
('snuffle-mat', 'snuffle-mat', 'Snuffle Mat', 'Barkplay', '{dog}', 'toys', NULL, 'all', 'all', NULL, FALSE, '{}',
 'Hide kibble in the fleece so your dog sniffs it out. Slows fast eaters.',
 'A washable fleece mat with layers to hide kibble or treats in. Sniffing out food keeps dogs busy and slows down dogs that eat too fast. Machine wash cold and dry flat.',
 '{}',
 NULL,
 NULL, 'India', '{photo:pSnuffle}',
 '{}', '{}', '{}',
 NULL,
 'active', 0, FALSE, 12, '6307', FALSE),

-- Padded Reflective Harness
('padded-harness', 'padded-reflective-harness', 'Padded Reflective Harness', 'Trailpaw', '{dog}', 'accessories', NULL, 'all', 'all', NULL, FALSE, '{}',
 'A padded chest harness with reflective trim for evening walks.',
 'A padded harness with a front and a back clip. The front clip gives more control for dogs that pull. Reflective trim helps you be seen on evening walks. Measure your dog’s chest before choosing a size.',
 '{}',
 NULL,
 NULL, 'India', '{photo:pHarness,photo:pHarness2,photo:harnessFolded}',
 '{}', '{}', '{}',
 NULL,
 'active', 0, FALSE, 12, '4201', FALSE),

-- Raised Steel Bowl Stand
('steel-bowl-stand', 'raised-steel-bowl-stand', 'Raised Steel Bowl Stand', 'Wagwell Home', '{dog,cat}', 'accessories', NULL, 'all', 'all', NULL, FALSE, '{}',
 'Two stainless steel bowls on a raised stand with non-slip feet.',
 'A raised stand with two stainless steel bowls that lift out for washing. The non-slip feet stop it sliding across the floor. Dishwasher safe.',
 '{}',
 NULL,
 NULL, 'India', '{photo:pStand}',
 '{}', '{}', '{}',
 NULL,
 'active', 0, FALSE, 18, '7323', FALSE),

-- Oatmeal & Aloe Shampoo
('oatmeal-shampoo', 'oatmeal-aloe-shampoo', 'Oatmeal & Aloe Shampoo', 'Paws & Polish', '{dog,cat}', 'grooming', NULL, 'all', 'all', NULL, FALSE, '{}',
 'A gentle, soap-free shampoo with oatmeal and aloe vera.',
 'A mild, soap-free shampoo with colloidal oatmeal and aloe vera for regular baths. Lather, leave for two minutes, then rinse well. If your pet has sore or broken skin, see a vet before bathing.',
 '{Water,"Colloidal oatmeal","Aloe vera","Coco glucoside",Glycerin}',
 NULL,
 NULL, 'India', '{photo:pShampoo,photo:shampoo}',
 '{}', '{}', '{}',
 NULL,
 'active', 0, TRUE, 18, '3307', FALSE),

-- Lemongrass Coat Spray
('lemongrass-spray', 'lemongrass-coat-spray', 'Lemongrass Coat Spray', 'Paws & Polish', '{dog}', 'grooming', NULL, 'all', 'all', NULL, FALSE, '{}',
 'A lemongrass and neem coat spray for freshening between baths.',
 'A light spray with lemongrass, cedarwood and neem to freshen your dog’s coat between baths. It is not a medicine and does not treat ticks or fleas — ask your vet about tick prevention. Not for use on cats.',
 '{Water,"Lemongrass oil","Cedarwood oil","Neem extract"}',
 NULL,
 NULL, 'India', '{photo:pSpray}',
 '{}', '{}', '{}',
 NULL,
 'active', 0, TRUE, 18, '3307', FALSE)
ON CONFLICT DO NOTHING;

------- Remaining Product Variants -------
INSERT INTO variants (id, product_id, sku, barcode, stock, low_stock_at, size, weight_kg, price, mrp) VALUES
-- Beef Senior
('sb-3', 'senior-beef', 'WG-SB-3', '890123456713', 25, 10, '3 kg', 3.0, 2249, 2599),
('sb-10', 'senior-beef', 'WG-SB-10', '890123456714', 0, 10, '10 kg', 10.0, 6299, 7499),

-- Turmeric & Parsley Biscuits
('tb-250', 'turmeric-biscuits', 'WG-TB-250', '890123456715', 12, 10, '250 g', 0.25, 349, 399),
('tb-500', 'turmeric-biscuits', 'WG-TB-500', '890123456716', 33, 10, '500 g', 0.5, 599, 699),

-- Salmon Oil Pump
('soil-250', 'salmon-oil', 'WG-SOIL-250', '890123456717', 60, 10, '250 ml', NULL, 699, 849),
('soil-500', 'salmon-oil', 'WG-SOIL-500', '890123456718', 21, 10, '500 ml', NULL, 1199, 1499),

-- Natural Rubber Chew Bone
('rb-m', 'rubber-bone', 'WG-RB-M', '890123456719', 5, 10, 'Medium · dogs 10–25 kg', NULL, 599, 799),
('rb-l', 'rubber-bone', 'WG-RB-L', '890123456720', 48, 10, 'Large · dogs over 25 kg', NULL, 799, 999),

-- Snuffle Mat
('sm-std', 'snuffle-mat', 'WG-SM-STD', '890123456721', 70, 10, 'Standard · 50 × 50 cm', NULL, 799, 1099),
('sm-xl', 'snuffle-mat', 'WG-SM-XL', '890123456722', 16, 10, 'Large · 75 × 75 cm', NULL, 1199, 1499),

-- Padded Reflective Harness
('ph-m', 'padded-harness', 'WG-PH-M', '890123456723', 36, 10, 'Medium · chest 52–68 cm', NULL, 1299, 1699),
('ph-l', 'padded-harness', 'WG-PH-L', '890123456724', 27, 10, 'Large · chest 65–82 cm', NULL, 1499, 1899),

-- Raised Steel Bowl Stand
('sbs-m', 'steel-bowl-stand', 'WG-SBS-M', '890123456725', 11, 10, 'Medium · 2 × 850 ml', NULL, 1499, 1899),
('sbs-l', 'steel-bowl-stand', 'WG-SBS-L', '890123456726', 50, 10, 'Large · 2 × 1400 ml', NULL, 1799, 2299),

-- Oatmeal & Aloe Shampoo
('os-250', 'oatmeal-shampoo', 'WG-OS-250', '890123456727', 44, 10, '250 ml', NULL, 399, 499),
('os-500', 'oatmeal-shampoo', 'WG-OS-500', '890123456728', 42, 10, '500 ml', NULL, 649, 799),

-- Lemongrass Coat Spray
('ls-200', 'lemongrass-spray', 'WG-LS-200', '890123456729', 120, 10, '200 ml spray', NULL, 449, 549),
('ls-500', 'lemongrass-spray', 'WG-LS-500', '890123456730', 18, 10, '500 ml refill', NULL, 799, 999)
ON CONFLICT DO NOTHING;

------- Remaining Coupons -------
INSERT INTO coupons (code, title, description, kind, value, max_discount, min_order, first_order_only, categories, product_ids, excludes_autoship, starts_at, ends_at, usage_limit, per_customer_limit, used, active) VALUES
('FLASH20', 'Flash deal: 20% off Salmon & Oats', 'On Salmon & Oats Adult while the flash deal runs.', 'percent', 20, NULL, 0, FALSE, '{}', '{salmon-oats}', TRUE, NULL, NULL, NULL, NULL, 0, TRUE),
('BISCUITBOGO', 'Buy 1 Get 1: Turmeric biscuits', 'Add two packs of Turmeric & Parsley Biscuits; the second is free.', 'bogo', 0, NULL, 0, FALSE, '{}', '{turmeric-biscuits}', TRUE, NULL, NULL, NULL, NULL, 0, TRUE),
('PUPPYSTART', '₹200 off the puppy starter bundle', '₹200 off orders of ₹1,999 or more.', 'flat', 200, NULL, 1999, FALSE, '{}', '{}', FALSE, NULL, NULL, NULL, 1, 0, TRUE)
ON CONFLICT DO NOTHING;
