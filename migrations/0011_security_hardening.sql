-- Security hardening (review of 11 Oct 2026):
--   * staff two-step sign-in (authenticator codes) with recovery codes,
--   * a list of each staff member's signed-in devices,
--   * riders choose their own password with a one-time setup code,
--   * one barcode belongs to one product size.

-- Two-step sign-in. Secrets are encrypted by the server (AES-256-GCM, MFA_ENCRYPTION_KEY), so a copy
-- of the database alone can't make codes. `mfa_last_step` stops a code from being used twice.
ALTER TABLE admin_users
    ADD COLUMN mfa_secret_enc  TEXT,
    ADD COLUMN mfa_pending_enc TEXT,
    ADD COLUMN mfa_enabled_at  TIMESTAMPTZ,
    ADD COLUMN mfa_last_step   BIGINT NOT NULL DEFAULT 0;

-- Ten single-use codes for a lost phone, stored only as hashes.
CREATE TABLE admin_recovery_codes (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    admin_id    UUID NOT NULL REFERENCES admin_users (id) ON DELETE CASCADE,
    code_hash   TEXT NOT NULL,
    used_at     TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX admin_recovery_codes_admin_idx ON admin_recovery_codes (admin_id);

-- Between "password right" and "code right": a short-lived, single-use step with limited tries.
CREATE TABLE admin_mfa_challenges (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    admin_id    UUID NOT NULL REFERENCES admin_users (id) ON DELETE CASCADE,
    token_hash  TEXT NOT NULL UNIQUE,
    client      TEXT NOT NULL CHECK (client IN ('web', 'mobile')),
    attempts    INT NOT NULL DEFAULT 0,
    expires_at  TIMESTAMPTZ NOT NULL,
    used_at     TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX admin_mfa_challenges_admin_idx ON admin_mfa_challenges (admin_id);

-- So staff can recognise their signed-in devices ("Chrome on Mac", "Wagwell Admin on iPhone").
ALTER TABLE admin_sessions ADD COLUMN user_agent TEXT;

-- A rider's first password (and any reset) is set by the rider with a one-time code from the shop,
-- so nobody else ever knows it.
CREATE TABLE rider_setup_codes (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    rider_id    UUID NOT NULL REFERENCES riders (id) ON DELETE CASCADE,
    code_hash   TEXT NOT NULL,
    attempts    INT NOT NULL DEFAULT 0,
    expires_at  TIMESTAMPTZ NOT NULL,
    used_at     TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX rider_setup_codes_rider_idx ON rider_setup_codes (rider_id);

-- One barcode, one product size. A blank barcode means "none".
UPDATE variants SET barcode = NULL WHERE barcode IS NOT NULL AND btrim(barcode) = '';
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM variants WHERE barcode IS NOT NULL GROUP BY barcode HAVING count(*) > 1) THEN
        -- Existing duplicates must be fixed by a person (the admin lists them); the API already
        -- refuses new ones. Run the CREATE below once they're gone.
        RAISE WARNING 'variants.barcode has duplicates, so its unique index was not created. Fix them (GET /api/v1/admin/barcodes/duplicates), then run: CREATE UNIQUE INDEX variants_barcode_unique ON variants (barcode) WHERE barcode IS NOT NULL;';
    ELSE
        CREATE UNIQUE INDEX variants_barcode_unique ON variants (barcode) WHERE barcode IS NOT NULL;
    END IF;
END $$;

ALTER TABLE admin_recovery_codes ENABLE ROW LEVEL SECURITY;
ALTER TABLE admin_mfa_challenges ENABLE ROW LEVEL SECURITY;
ALTER TABLE rider_setup_codes    ENABLE ROW LEVEL SECURITY;
