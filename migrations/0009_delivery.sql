-- Own delivery riders: their accounts and sessions, which orders they carry, where they are now,
-- and the 4-digit handover code the customer reads out at the door.
--
-- Riders are a separate table from staff on purpose: a rider can never reach the admin (customers,
-- money, catalogue), and a staff session can never act as a rider.

CREATE TABLE riders (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    mobile          TEXT NOT NULL UNIQUE CHECK (mobile ~ '^[6-9][0-9]{9}$'),
    name            TEXT NOT NULL,
    password_hash   TEXT NOT NULL,
    failed_attempts INT  NOT NULL DEFAULT 0,
    locked_until    TIMESTAMPTZ,
    active          BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_login_at   TIMESTAMPTZ
);

CREATE TABLE rider_sessions (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    rider_id    UUID NOT NULL REFERENCES riders (id) ON DELETE CASCADE,
    token_hash  TEXT NOT NULL UNIQUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL,
    revoked_at  TIMESTAMPTZ
);
CREATE INDEX rider_sessions_rider_idx ON rider_sessions (rider_id);

-- Only the latest position is kept (no location history).
CREATE TABLE rider_locations (
    rider_id    UUID PRIMARY KEY REFERENCES riders (id) ON DELETE CASCADE,
    lat         DOUBLE PRECISION NOT NULL CHECK (lat BETWEEN -90 AND 90),
    lng         DOUBLE PRECISION NOT NULL CHECK (lng BETWEEN -180 AND 180),
    accuracy_m  REAL CHECK (accuracy_m >= 0),
    heading     REAL CHECK (heading >= 0 AND heading < 360),
    speed_mps   REAL CHECK (speed_mps >= 0),
    at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE orders
    ADD COLUMN rider_id                UUID REFERENCES riders (id),
    ADD COLUMN delivery_code           TEXT CHECK (delivery_code ~ '^[0-9]{4}$'),
    ADD COLUMN delivery_code_attempts  INT NOT NULL DEFAULT 0;
CREATE INDEX orders_rider_idx ON orders (rider_id, status) WHERE rider_id IS NOT NULL;

ALTER TABLE order_events DROP CONSTRAINT order_events_actor_type_check;
ALTER TABLE order_events ADD CONSTRAINT order_events_actor_type_check
    CHECK (actor_type IN ('customer', 'admin', 'system', 'gateway', 'rider'));

ALTER TABLE audit_log DROP CONSTRAINT audit_log_actor_type_check;
ALTER TABLE audit_log ADD CONSTRAINT audit_log_actor_type_check
    CHECK (actor_type IN ('admin', 'customer', 'system', 'rider'));

-- Same lock-down as 0002: only the API server may read these.
ALTER TABLE riders          ENABLE ROW LEVEL SECURITY;
ALTER TABLE rider_sessions  ENABLE ROW LEVEL SECURITY;
ALTER TABLE rider_locations ENABLE ROW LEVEL SECURITY;
