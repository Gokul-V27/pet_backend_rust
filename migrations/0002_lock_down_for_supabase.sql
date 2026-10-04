-- Supabase publishes everything in the `public` schema through its REST/GraphQL API to the
-- `anon` and `authenticated` roles (the anon key is public by design). Only this API server may
-- touch these tables, so: row-level security on (no policies = no rows for those roles) and all
-- their grants removed, now and for tables added later. The server connects as the owner, which
-- bypasses RLS. On plain PostgreSQL (local tests) the Supabase roles don't exist and this is a no-op
-- apart from enabling RLS.

ALTER TABLE customers         ENABLE ROW LEVEL SECURITY;
ALTER TABLE otp_codes         ENABLE ROW LEVEL SECURITY;
ALTER TABLE customer_sessions ENABLE ROW LEVEL SECURITY;
ALTER TABLE admin_users       ENABLE ROW LEVEL SECURITY;
ALTER TABLE admin_sessions    ENABLE ROW LEVEL SECURITY;
ALTER TABLE audit_log         ENABLE ROW LEVEL SECURITY;

DO $$
DECLARE r TEXT;
BEGIN
    FOREACH r IN ARRAY ARRAY['anon', 'authenticated'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = r) THEN
            EXECUTE format('REVOKE ALL ON ALL TABLES IN SCHEMA public FROM %I', r);
            EXECUTE format('REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM %I', r);
            EXECUTE format('REVOKE ALL ON ALL FUNCTIONS IN SCHEMA public FROM %I', r);
            EXECUTE format('ALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE ALL ON TABLES FROM %I', r);
            EXECUTE format('ALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE ALL ON SEQUENCES FROM %I', r);
            EXECUTE format('ALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE ALL ON FUNCTIONS FROM %I', r);
        END IF;
    END LOOP;
END $$;
