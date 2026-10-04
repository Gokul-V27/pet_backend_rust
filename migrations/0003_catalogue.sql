-- Catalogue: categories, products, variants, reviews, coupons, offers, inventory,
-- sample campaigns, message templates, care templates, and a key-value settings table.
-- All data the admin website currently keeps in localStorage.

------- Categories -------

CREATE TABLE categories (
    slug        TEXT PRIMARY KEY,
    label       TEXT NOT NULL,
    blurb       TEXT NOT NULL DEFAULT '',
    photo       TEXT NOT NULL DEFAULT '',
    tone        TEXT NOT NULL DEFAULT 'fog' CHECK (tone IN ('pink', 'sky', 'mint', 'fog')),
    parent_slug TEXT REFERENCES categories (slug) ON DELETE SET NULL,
    active      BOOLEAN NOT NULL DEFAULT TRUE,
    sort        INT NOT NULL DEFAULT 0,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

------- Products -------

CREATE TABLE products (
    id                  TEXT PRIMARY KEY,
    slug                TEXT NOT NULL UNIQUE,
    name                TEXT NOT NULL,
    brand               TEXT NOT NULL,
    species             TEXT[] NOT NULL DEFAULT '{}',
    category_slug       TEXT NOT NULL REFERENCES categories (slug) ON DELETE RESTRICT,
    subcategory         TEXT,
    life_stage          TEXT NOT NULL DEFAULT 'all' CHECK (life_stage IN ('puppy', 'adult', 'senior', 'all')),
    breed_size          TEXT NOT NULL DEFAULT 'all' CHECK (breed_size IN ('small', 'medium', 'large', 'all')),
    diet                TEXT CHECK (diet IN ('veg', 'non-veg')),
    grain_free          BOOLEAN NOT NULL DEFAULT FALSE,
    allergens           TEXT[] NOT NULL DEFAULT '{}',
    summary             TEXT NOT NULL DEFAULT '',
    description         TEXT NOT NULL DEFAULT '',
    ingredients         TEXT[] NOT NULL DEFAULT '{}',
    nutrition           JSONB,
    best_before         TEXT,
    country_of_origin   TEXT NOT NULL DEFAULT 'India',
    images              TEXT[] NOT NULL DEFAULT '{}',
    benefits            TEXT[] NOT NULL DEFAULT '{}',
    videos              TEXT[] NOT NULL DEFAULT '{}',
    suitable_breeds     TEXT[] NOT NULL DEFAULT '{}',
    feeding_instructions TEXT,
    status              TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'draft', 'inactive')),
    popularity          INT NOT NULL DEFAULT 0,
    autoship_eligible   BOOLEAN NOT NULL DEFAULT FALSE,
    gst_rate_pct        INT NOT NULL DEFAULT 18,
    hsn                 TEXT NOT NULL DEFAULT '',
    is_new              BOOLEAN NOT NULL DEFAULT FALSE,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX products_category_idx ON products (category_slug);
CREATE INDEX products_status_idx ON products (status);

------- Variants -------

CREATE TABLE variants (
    id          TEXT PRIMARY KEY,
    product_id  TEXT NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    sku         TEXT NOT NULL,
    barcode     TEXT,
    stock       INT NOT NULL DEFAULT 0 CHECK (stock >= 0),
    low_stock_at INT NOT NULL DEFAULT 10,
    size        TEXT NOT NULL,
    weight_kg   DOUBLE PRECISION,
    price       INT NOT NULL CHECK (price > 0),
    mrp         INT NOT NULL CHECK (mrp > 0),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX variants_product_idx ON variants (product_id);
CREATE UNIQUE INDEX variants_sku_idx ON variants (sku);

------- Reviews -------

CREATE TABLE reviews (
    id          TEXT PRIMARY KEY DEFAULT 'rev-' || substr(gen_random_uuid()::text, 1, 8),
    product_id  TEXT NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    author      TEXT NOT NULL,
    pet_label   TEXT NOT NULL DEFAULT '',
    pet_species TEXT NOT NULL DEFAULT 'dog',
    rating      SMALLINT NOT NULL CHECK (rating BETWEEN 1 AND 5),
    title       TEXT NOT NULL DEFAULT '',
    body        TEXT NOT NULL DEFAULT '',
    date        TEXT NOT NULL DEFAULT to_char(now(), 'YYYY-MM-DD'),
    helpful     INT NOT NULL DEFAULT 0,
    order_id    TEXT,
    reply       JSONB,
    media       TEXT[] NOT NULL DEFAULT '{}',
    status      TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'approved', 'rejected', 'hidden')),
    featured    BOOLEAN NOT NULL DEFAULT FALSE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX reviews_product_idx ON reviews (product_id);
CREATE INDEX reviews_status_idx ON reviews (status);

------- Coupons -------

CREATE TABLE coupons (
    code                TEXT PRIMARY KEY,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    kind                TEXT NOT NULL CHECK (kind IN ('percent', 'flat', 'free-shipping', 'bogo')),
    value               INT NOT NULL DEFAULT 0,
    max_discount        INT,
    min_order           INT NOT NULL DEFAULT 0,
    first_order_only    BOOLEAN NOT NULL DEFAULT FALSE,
    categories          TEXT[] NOT NULL DEFAULT '{}',
    product_ids         TEXT[] NOT NULL DEFAULT '{}',
    excludes_autoship   BOOLEAN NOT NULL DEFAULT FALSE,
    starts_at           TEXT,
    ends_at             TEXT,
    usage_limit         INT,
    per_customer_limit  INT,
    used                INT NOT NULL DEFAULT 0,
    active              BOOLEAN NOT NULL DEFAULT TRUE,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

------- Offers -------

CREATE TABLE offers (
    id          TEXT PRIMARY KEY DEFAULT 'offer-' || substr(gen_random_uuid()::text, 1, 8),
    kind        TEXT NOT NULL DEFAULT 'discount' CHECK (kind IN ('limited', 'bogo', 'discount', 'new', 'seasonal', 'bundle')),
    title       TEXT NOT NULL,
    line        TEXT NOT NULL DEFAULT '',
    image       TEXT NOT NULL DEFAULT '',
    badge       TEXT NOT NULL DEFAULT '',
    link        TEXT NOT NULL DEFAULT '',
    cta         TEXT NOT NULL DEFAULT 'Shop now',
    coupon_code TEXT,
    species     TEXT NOT NULL DEFAULT 'all' CHECK (species IN ('dog', 'cat', 'all')),
    food        BOOLEAN NOT NULL DEFAULT FALSE,
    flash       BOOLEAN NOT NULL DEFAULT FALSE,
    starts_at   TEXT NOT NULL DEFAULT to_char(now(), 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    ends_at     TEXT,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    active      BOOLEAN NOT NULL DEFAULT TRUE,
    in_loader   BOOLEAN NOT NULL DEFAULT FALSE,
    poster_line TEXT,
    poster_big  TEXT,
    poster_image TEXT
);

------- Inventory transactions -------

CREATE TABLE inventory_txns (
    id          TEXT PRIMARY KEY DEFAULT 'txn-' || substr(gen_random_uuid()::text, 1, 8),
    variant_id  TEXT NOT NULL REFERENCES variants (id) ON DELETE CASCADE,
    product_id  TEXT NOT NULL,
    type        TEXT NOT NULL CHECK (type IN ('opening', 'addition', 'sale', 'return', 'adjustment')),
    qty         INT NOT NULL,
    before_qty  INT NOT NULL,
    after_qty   INT NOT NULL,
    reason      TEXT NOT NULL DEFAULT '',
    by_whom     TEXT NOT NULL DEFAULT 'System',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX inventory_txns_variant_idx ON inventory_txns (variant_id);
CREATE INDEX inventory_txns_created_idx ON inventory_txns (created_at DESC);

------- Sample campaigns -------

CREATE TABLE sample_campaigns (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    product_id      TEXT NOT NULL,
    size            TEXT NOT NULL DEFAULT '',
    starts_at       TEXT NOT NULL,
    ends_at         TEXT NOT NULL,
    max_claims      INT NOT NULL DEFAULT 1000,
    claims          INT NOT NULL DEFAULT 0,
    per_household   INT NOT NULL DEFAULT 1,
    delivery_fee    INT NOT NULL DEFAULT 0,
    first_order_only BOOLEAN NOT NULL DEFAULT TRUE,
    active          BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

------- Message templates -------

CREATE TABLE message_templates (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    event       TEXT NOT NULL,
    channel     TEXT NOT NULL CHECK (channel IN ('whatsapp', 'email', 'website')),
    body        TEXT NOT NULL DEFAULT '',
    approval    TEXT NOT NULL DEFAULT 'draft' CHECK (approval IN ('approved', 'pending', 'draft')),
    active      BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

------- Care templates (pet reminders) -------

CREATE TABLE care_templates (
    id                  TEXT PRIMARY KEY,
    kind                TEXT NOT NULL CHECK (kind IN ('birthday', 'vaccine', 'deworming')),
    name                TEXT NOT NULL,
    species             TEXT[] NOT NULL DEFAULT '{}',
    first_due_weeks     INT NOT NULL DEFAULT 52,
    repeat_months       INT NOT NULL DEFAULT 0,
    remind_days_before  INT NOT NULL DEFAULT 7,
    channels            TEXT[] NOT NULL DEFAULT '{website}',
    message             TEXT NOT NULL DEFAULT '',
    active              BOOLEAN NOT NULL DEFAULT TRUE,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

------- Site-wide admin settings (key-value) -------

CREATE TABLE admin_settings (
    key         TEXT PRIMARY KEY,
    value       JSONB NOT NULL DEFAULT '{}'::jsonb,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

------- RLS for Supabase -------

ALTER TABLE categories       ENABLE ROW LEVEL SECURITY;
ALTER TABLE products         ENABLE ROW LEVEL SECURITY;
ALTER TABLE variants         ENABLE ROW LEVEL SECURITY;
ALTER TABLE reviews          ENABLE ROW LEVEL SECURITY;
ALTER TABLE coupons          ENABLE ROW LEVEL SECURITY;
ALTER TABLE offers           ENABLE ROW LEVEL SECURITY;
ALTER TABLE inventory_txns   ENABLE ROW LEVEL SECURITY;
ALTER TABLE sample_campaigns ENABLE ROW LEVEL SECURITY;
ALTER TABLE message_templates ENABLE ROW LEVEL SECURITY;
ALTER TABLE care_templates   ENABLE ROW LEVEL SECURITY;
ALTER TABLE admin_settings   ENABLE ROW LEVEL SECURITY;
