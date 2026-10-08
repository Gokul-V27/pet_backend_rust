-- Stock log ids were 'txn-' + 8 hex characters: after a few tens of thousands of stock changes two
-- would collide, and because every order line writes one, that collision would fail a real checkout.
-- New ids use the whole UUID. Existing ids stay as they are (they are already unique).
ALTER TABLE inventory_txns ALTER COLUMN id SET DEFAULT 'txn-' || gen_random_uuid()::text;
