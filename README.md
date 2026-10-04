# Wagwell API (Rust)

One server for **both** websites — the shop (`customer-website/`) and the staff admin (`admin-website/`).
axum 0.8 · tokio · sqlx (PostgreSQL) · Argon2id · cookie sessions. Plan: `../RUST_BACKEND_PLAN.md`.

## Run it

```bash
createdb wagwell                      # once, with PostgreSQL running
cp .env.example .env                  # then edit DATABASE_URL and OTP_PEPPER (openssl rand -hex 32)
set -a; source .env; set +a
cargo run                             # migrates the database, serves on 127.0.0.1:8080
```

First staff account (there is no staff sign-up page). The password goes in on standard input, so it never lands in shell history:

```bash
read -rs P && echo "$P" | cargo run -- create-admin you@wagwell.in "Your Name" super_admin
```

Shop sign-in in development: ask for a code, then read it in the server log (`DEV ONLY: one-time code`). Production refuses to start with that switch on; a real SMS provider (MSG91, DLT-registered) is the next step.

## Supabase

Use the **session pooler** URL (port 5432) as `DATABASE_URL` — see `.env.example`. The server only needs the
database connection: **the Supabase anon / publishable / secret / service_role keys are not used** and must never
be put in either website. Migration `0002` turns on row-level security and removes the `anon` and
`authenticated` roles' access, so Supabase's public REST API can't read these tables even with the anon key.

## Pictures (Supabase Storage)

Set the `S3_*` and `STORAGE_PUBLIC_URL` values in `.env` (see `.env.example`). The S3 keys are server-only: they can
read, write and delete every file. For shoppers to see product pictures, the bucket must be **Public** in Supabase
(Storage → `image_pet` → Edit bucket → Public). Public means anyone can *view* a file if they know its link; uploading
and deleting still need the keys, which only this server has.

Live check against the real bucket (uploads a 1-pixel PNG, reads it back, deletes it):

```bash
set -a; source .env; set +a; cargo test --test storage_live -- --ignored --nocapture
```

## Check it

```bash
cargo test                                            # unit tests + HTTP tests (no database needed)
DATABASE_URL=postgres://USER:PASS@localhost:5432/postgres cargo test --test db -- --ignored   # sign-in flows; LOCAL Postgres only
cargo clippy --all-targets && cargo fmt --check
```

## What exists (phase 1)

| | |
|---|---|
| `GET /health`, `/ready` | alive / can reach the database |
| `POST /api/v1/auth/otp/request`, `/otp/verify`, `/logout` · `GET /auth/me` | shop sign-in with a 6-digit code (5 min, 5 tries, 5 codes/hour, 1 per 30 s) |
| `POST /api/v1/admin/auth/login`, `/logout` · `GET /admin/auth/me` | staff email + password; 5 wrong tries lock 15 min; 8 h session, 30 min idle |
| audit log | append-only table; every staff sign-in, failure and sign-out |
| `POST /api/v1/admin/images` · `DELETE /api/v1/admin/images/{key}` | staff picture upload (multipart `file`, optional `folder`: products / categories / offers / reviews) → `{ key, url }`; JPEG / PNG / WebP only, checked from the file's bytes; 5 MB max; random names; roles Super Admin, Admin, Inventory Manager; audit-logged |

Security basics already in: secrets only from the environment · tokens/codes stored as hashes · HttpOnly + SameSite cookies (`Secure` outside development) · CORS allow-list of the two sites (never `*`) · a required `X-Wagwell-Client: web` header on every changing request · no-store / nosniff / no-frame headers · errors never leak internals · same answer and timing for unknown email vs wrong password.
