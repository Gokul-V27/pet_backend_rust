-- The staff phone app: scan a pack, receive stock with its batch and expiry, and sign in with a
-- bearer token instead of a cookie.

-- Which kind of client made a staff session. Website sessions end after 30 minutes idle; a phone
-- session lasts one shift (see auth.rs), because the shop floor phone is locked by its own screen lock.
ALTER TABLE admin_sessions
    ADD COLUMN client TEXT NOT NULL DEFAULT 'web' CHECK (client IN ('web', 'mobile'));

-- One goods-in: a supplier's delivery received as a whole, or a single pack scanned on the phone.
-- The phone sends an Idempotency-Key, so a double tap or a retry after a lost answer never adds the
-- stock twice: the same key returns the stored result instead.
CREATE TABLE inventory_receipts (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    admin_id         UUID NOT NULL REFERENCES admin_users (id),
    idempotency_key  TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 8 AND 100),
    request_hash     TEXT NOT NULL,
    supplier_ref     TEXT NOT NULL DEFAULT '',
    note             TEXT NOT NULL DEFAULT '',
    result           JSONB NOT NULL DEFAULT '{}',
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (admin_id, idempotency_key)
);
CREATE INDEX inventory_receipts_created_idx ON inventory_receipts (created_at DESC);

-- Batch, dates and cost on the stock log line that received them (food is sold first-expiry-first-out).
ALTER TABLE inventory_txns
    ADD COLUMN receipt_id   UUID REFERENCES inventory_receipts (id),
    ADD COLUMN batch_no     TEXT CHECK (length(batch_no) <= 60),
    ADD COLUMN mfg_date     DATE,
    ADD COLUMN expiry_date  DATE,
    ADD COLUMN unit_cost    INT CHECK (unit_cost >= 0),
    ADD CONSTRAINT inventory_txns_dates_check CHECK (mfg_date IS NULL OR expiry_date IS NULL OR mfg_date <= expiry_date);
CREATE INDEX inventory_txns_expiry_idx ON inventory_txns (expiry_date) WHERE expiry_date IS NOT NULL;
CREATE INDEX inventory_txns_receipt_idx ON inventory_txns (receipt_id) WHERE receipt_id IS NOT NULL;

-- A scan looks the pack up by its barcode.
CREATE INDEX variants_barcode_idx ON variants (barcode) WHERE barcode IS NOT NULL;

-- Same lock-down as 0002: only the API server may read this.
ALTER TABLE inventory_receipts ENABLE ROW LEVEL SECURITY;
