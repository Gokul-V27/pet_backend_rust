-- Customers sign in with a one-time code sent to their mobile; staff sign in with email + password.
-- Tokens and codes are stored only as hashes. Money, orders and the catalogue come in later migrations.

CREATE TABLE customers (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    mobile      TEXT NOT NULL UNIQUE CHECK (mobile ~ '^[6-9][0-9]{9}$'),
    name        TEXT,
    email       TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE otp_codes (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    mobile       TEXT NOT NULL,
    code_hash    TEXT NOT NULL,
    attempts     INT  NOT NULL DEFAULT 0,
    expires_at   TIMESTAMPTZ NOT NULL,
    consumed_at  TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX otp_codes_mobile_created_idx ON otp_codes (mobile, created_at DESC);

CREATE TABLE customer_sessions (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    customer_id UUID NOT NULL REFERENCES customers (id) ON DELETE CASCADE,
    token_hash  TEXT NOT NULL UNIQUE,
    user_agent  TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL,
    revoked_at  TIMESTAMPTZ
);
CREATE INDEX customer_sessions_customer_idx ON customer_sessions (customer_id);

CREATE TABLE admin_users (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email           TEXT NOT NULL,
    name            TEXT NOT NULL,
    role            TEXT NOT NULL CHECK (role IN ('super_admin', 'admin', 'order_manager', 'inventory_manager', 'support')),
    password_hash   TEXT NOT NULL,
    failed_attempts INT  NOT NULL DEFAULT 0,
    locked_until    TIMESTAMPTZ,
    active          BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_login_at   TIMESTAMPTZ
);
CREATE UNIQUE INDEX admin_users_email_idx ON admin_users (lower(email));

CREATE TABLE admin_sessions (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    admin_id     UUID NOT NULL REFERENCES admin_users (id) ON DELETE CASCADE,
    token_hash   TEXT NOT NULL UNIQUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ NOT NULL,
    revoked_at   TIMESTAMPTZ
);
CREATE INDEX admin_sessions_admin_idx ON admin_sessions (admin_id);

-- Who did what, when. Append-only: updates and deletes are blocked.
CREATE TABLE audit_log (
    id          BIGSERIAL PRIMARY KEY,
    at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    actor_type  TEXT NOT NULL CHECK (actor_type IN ('admin', 'customer', 'system')),
    actor_id    UUID,
    action      TEXT NOT NULL,
    target      TEXT,
    detail      JSONB NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX audit_log_at_idx ON audit_log (at DESC);

CREATE FUNCTION audit_log_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'audit_log is append-only';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER audit_log_no_change
    BEFORE UPDATE OR DELETE ON audit_log
    FOR EACH ROW EXECUTE FUNCTION audit_log_append_only();
