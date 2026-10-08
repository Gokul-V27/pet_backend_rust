-- Idempotency: remember a fingerprint of what each order request contained, so replaying the same
-- Idempotency-Key with a different basket, address or slot is refused instead of silently returning
-- the old order. Nullable: orders created before this migration have none (and are replayed as before).
ALTER TABLE orders ADD COLUMN request_hash TEXT;
