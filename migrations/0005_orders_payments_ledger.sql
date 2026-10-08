-- Orders, payments, refunds and the double-entry ledger (PawLedger core).
--
-- Rules the database itself enforces, so no code path can break them:
--   * money is whole paise (BIGINT), never fractions;
--   * every journal entry balances (total debits = total credits), checked when the transaction commits;
--   * posted journal entries and lines can't be edited or deleted — mistakes are fixed with a reversing entry;
--   * one order per (customer, idempotency key), one entry per (source, purpose), one row per gateway id,
--     so retries and replayed webhooks can never double-count.

------- Chart of accounts -------

CREATE TABLE ledger_accounts (
    code    TEXT PRIMARY KEY,
    name    TEXT NOT NULL,
    kind    TEXT NOT NULL CHECK (kind IN ('asset', 'liability', 'equity', 'income', 'expense'))
);

INSERT INTO ledger_accounts (code, name, kind) VALUES
    ('1000', 'Cash in hand (cash on delivery collected)', 'asset'),
    ('1010', 'Payment gateway clearing (Razorpay)',      'asset'),
    ('2100', 'GST output payable',                        'liability'),
    ('2200', 'Donations owed to animal charity',          'liability'),
    ('2400', 'Customer advances (paid, not yet delivered)', 'liability'),
    ('4000', 'Product sales',                             'income'),
    ('4100', 'Delivery charges',                          'income'),
    ('4200', 'Cash-on-delivery fees',                     'income'),
    ('4500', 'Sales returns',                             'expense');

------- Journal -------

CREATE TABLE journal_entries (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    posted_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    memo        TEXT NOT NULL,
    -- What caused it, e.g. ('order', <order id>, 'delivered') or ('refund', <refund id>, 'refund').
    source_type TEXT NOT NULL,
    source_id   UUID NOT NULL,
    purpose     TEXT NOT NULL,
    created_by  TEXT NOT NULL DEFAULT 'system',
    UNIQUE (source_type, source_id, purpose)
);
CREATE INDEX journal_entries_posted_idx ON journal_entries (posted_at DESC);

CREATE TABLE journal_lines (
    id           BIGSERIAL PRIMARY KEY,
    entry_id     UUID NOT NULL REFERENCES journal_entries (id),
    account_code TEXT NOT NULL REFERENCES ledger_accounts (code),
    debit_paise  BIGINT NOT NULL DEFAULT 0 CHECK (debit_paise >= 0),
    credit_paise BIGINT NOT NULL DEFAULT 0 CHECK (credit_paise >= 0),
    CHECK ((debit_paise > 0) <> (credit_paise > 0))
);
CREATE INDEX journal_lines_entry_idx ON journal_lines (entry_id);
CREATE INDEX journal_lines_account_idx ON journal_lines (account_code);

-- Balanced or the whole transaction fails.
CREATE FUNCTION journal_entry_must_balance() RETURNS trigger
LANGUAGE plpgsql SET search_path = public AS $$
DECLARE diff BIGINT;
BEGIN
    SELECT COALESCE(SUM(debit_paise), 0) - COALESCE(SUM(credit_paise), 0) INTO diff
      FROM journal_lines WHERE entry_id = NEW.entry_id;
    IF diff <> 0 THEN
        RAISE EXCEPTION 'journal entry % does not balance (off by % paise)', NEW.entry_id, diff;
    END IF;
    RETURN NULL;
END $$;

CREATE CONSTRAINT TRIGGER journal_lines_balanced
    AFTER INSERT ON journal_lines
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION journal_entry_must_balance();

-- Append-only.
CREATE FUNCTION journal_is_append_only() RETURNS trigger
LANGUAGE plpgsql SET search_path = public AS $$
BEGIN
    RAISE EXCEPTION 'posted journal rows are permanent; post a reversing entry instead';
END $$;

CREATE TRIGGER journal_entries_no_change BEFORE UPDATE OR DELETE ON journal_entries
    FOR EACH ROW EXECUTE FUNCTION journal_is_append_only();
CREATE TRIGGER journal_lines_no_change BEFORE UPDATE OR DELETE ON journal_lines
    FOR EACH ROW EXECUTE FUNCTION journal_is_append_only();

------- Orders -------

CREATE SEQUENCE order_number_seq START 100001;

CREATE TABLE orders (
    id                      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    number                  TEXT NOT NULL UNIQUE,
    customer_id             UUID NOT NULL REFERENCES customers (id),
    idempotency_key         TEXT NOT NULL,
    status                  TEXT NOT NULL CHECK (status IN (
                                'pending', 'confirmed', 'processing', 'packed', 'shipped', 'out-for-delivery',
                                'delivered', 'cancelled', 'returned', 'payment-failed')),
    payment_method          TEXT NOT NULL CHECK (payment_method IN ('upi', 'card', 'netbanking', 'wallet', 'cod')),
    payment_status          TEXT NOT NULL CHECK (payment_status IN ('pending', 'paid', 'cod-due', 'failed', 'refunded', 'partially-refunded')),
    address                 JSONB NOT NULL,
    slot_date               DATE NOT NULL,
    slot_label              TEXT NOT NULL,
    delivery_kind           TEXT NOT NULL CHECK (delivery_kind IN ('standard', 'express')),
    mrp_total_paise         BIGINT NOT NULL CHECK (mrp_total_paise >= 0),
    items_total_paise       BIGINT NOT NULL CHECK (items_total_paise >= 0),
    autoship_savings_paise  BIGINT NOT NULL DEFAULT 0 CHECK (autoship_savings_paise >= 0),
    coupon_code             TEXT,
    coupon_discount_paise   BIGINT NOT NULL DEFAULT 0 CHECK (coupon_discount_paise >= 0),
    delivery_paise          BIGINT NOT NULL DEFAULT 0 CHECK (delivery_paise >= 0),
    cod_fee_paise           BIGINT NOT NULL DEFAULT 0 CHECK (cod_fee_paise >= 0),
    donation_paise          BIGINT NOT NULL DEFAULT 0 CHECK (donation_paise >= 0),
    wallet_used_paise       BIGINT NOT NULL DEFAULT 0 CHECK (wallet_used_paise >= 0),
    total_paise             BIGINT NOT NULL CHECK (total_paise >= 0),
    gst_paise               BIGINT NOT NULL DEFAULT 0 CHECK (gst_paise >= 0),
    whatsapp_updates        BOOLEAN NOT NULL DEFAULT FALSE,
    courier                 TEXT,
    awb                     TEXT,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (customer_id, idempotency_key)
);
CREATE INDEX orders_customer_idx ON orders (customer_id, created_at DESC);
CREATE INDEX orders_status_idx ON orders (status, created_at DESC);

CREATE TABLE order_items (
    id               BIGSERIAL PRIMARY KEY,
    order_id         UUID NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    product_id       TEXT NOT NULL,
    variant_id       TEXT NOT NULL,
    name             TEXT NOT NULL,
    size             TEXT NOT NULL,
    qty              INT NOT NULL CHECK (qty > 0),
    unit_price_paise BIGINT NOT NULL CHECK (unit_price_paise >= 0),
    mrp_paise        BIGINT NOT NULL CHECK (mrp_paise >= 0),
    gst_rate_pct     INT NOT NULL CHECK (gst_rate_pct BETWEEN 0 AND 28),
    hsn              TEXT NOT NULL DEFAULT '',
    line_total_paise BIGINT NOT NULL CHECK (line_total_paise >= 0),
    -- This line's share of the order discount and of GST, fixed at order time for invoices and GST reports.
    discount_paise   BIGINT NOT NULL DEFAULT 0 CHECK (discount_paise >= 0),
    gst_paise        BIGINT NOT NULL DEFAULT 0 CHECK (gst_paise >= 0),
    autoship         BOOLEAN NOT NULL DEFAULT FALSE,
    frequency_days   INT
);
CREATE INDEX order_items_order_idx ON order_items (order_id);

CREATE TABLE order_events (
    id          BIGSERIAL PRIMARY KEY,
    order_id    UUID NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    status      TEXT NOT NULL,
    note        TEXT,
    actor_type  TEXT NOT NULL CHECK (actor_type IN ('customer', 'admin', 'system', 'gateway')),
    actor_id    UUID,
    at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX order_events_order_idx ON order_events (order_id, at);

------- Payments and refunds -------

CREATE TABLE payments (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    order_id            UUID NOT NULL REFERENCES orders (id),
    provider            TEXT NOT NULL CHECK (provider IN ('razorpay', 'cod')),
    method              TEXT NOT NULL,
    amount_paise        BIGINT NOT NULL CHECK (amount_paise > 0),
    refunded_paise      BIGINT NOT NULL DEFAULT 0 CHECK (refunded_paise >= 0),
    status              TEXT NOT NULL CHECK (status IN ('pending', 'captured', 'failed', 'partially_refunded', 'refunded')),
    gateway_order_id    TEXT UNIQUE,
    gateway_payment_id  TEXT UNIQUE,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (refunded_paise <= amount_paise)
);
CREATE INDEX payments_order_idx ON payments (order_id);

CREATE TABLE refunds (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    payment_id         UUID NOT NULL REFERENCES payments (id),
    amount_paise       BIGINT NOT NULL CHECK (amount_paise > 0),
    reason             TEXT NOT NULL,
    status             TEXT NOT NULL CHECK (status IN ('pending', 'processed', 'failed')),
    gateway_refund_id  TEXT UNIQUE,
    created_by         UUID,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX refunds_payment_idx ON refunds (payment_id);

-- Every webhook delivery we've handled, so a replay is recognised and ignored.
CREATE TABLE webhook_events (
    id           TEXT PRIMARY KEY,
    provider     TEXT NOT NULL,
    event        TEXT NOT NULL,
    received_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

------- RLS for Supabase (same lock-down as 0002/0003) -------

ALTER TABLE ledger_accounts ENABLE ROW LEVEL SECURITY;
ALTER TABLE journal_entries ENABLE ROW LEVEL SECURITY;
ALTER TABLE journal_lines   ENABLE ROW LEVEL SECURITY;
ALTER TABLE orders          ENABLE ROW LEVEL SECURITY;
ALTER TABLE order_items     ENABLE ROW LEVEL SECURITY;
ALTER TABLE order_events    ENABLE ROW LEVEL SECURITY;
ALTER TABLE payments        ENABLE ROW LEVEL SECURITY;
ALTER TABLE refunds         ENABLE ROW LEVEL SECURITY;
ALTER TABLE webhook_events  ENABLE ROW LEVEL SECURITY;

DO $$
DECLARE r TEXT;
BEGIN
    FOREACH r IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = r) THEN
            EXECUTE format('REVOKE ALL ON ALL TABLES IN SCHEMA public FROM %I', r);
            EXECUTE format('REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM %I', r);
            EXECUTE format('REVOKE ALL ON ALL FUNCTIONS IN SCHEMA public FROM %I', r);
        END IF;
    END LOOP;
END $$;
