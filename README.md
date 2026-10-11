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

Shop sign-in in development: ask for a code, then read it in the server log (`DEV ONLY: one-time code`). Production refuses to start with that switch on and sends codes through MSG91 (see "Sign-in codes" below).

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

## Orders, payments and the ledger (PawLedger core)

Money is whole paise (`BIGINT`). **The server prices every order** (a port of the shop's `computePricing`); the browser only
says what was chosen and the total it showed — if they differ the order is refused with `price_changed` and nothing is charged.

| | |
|---|---|
| `POST /api/v1/orders/quote` | price a cart for the signed-in customer (no stock taken) |
| `POST /api/v1/orders` + `Idempotency-Key` | place an order: price, check pincode/COD rules and the delivery slot, take stock, open the Razorpay order. The same key and the same request returns the same order; the same key with a different request is `422 idempotency_key_reused`; a key whose order was closed is `409 order_closed` |
| `GET /api/v1/orders`, `GET /orders/{number}`, `POST /orders/{number}/cancel` | the customer's own orders only (anyone else's looks missing) |
| `POST /api/v1/payments/razorpay/verify` | Checkout success: HMAC check, then the order is paid |
| `POST /api/v1/payments/razorpay/webhook` | no website header; HMAC of the raw body; each event id handled once (`payment.captured`, `payment.failed`, `refund.processed`, `refund.failed`). A failed attempt only adds a note — the customer can retry in the same Razorpay window |
| `GET /api/v1/admin/orders`, `GET /admin/orders/{number}` | all staff |
| `POST /admin/orders/{number}/status`, `/shipping` | Super Admin, Admin, Order Manager; cancelling a paid order refunds it in full |
| `POST /admin/orders/{number}/refund` | Super Admin, Admin; full or partial, only on a cancelled or delivered order, never more than was paid (the amount is reserved before Razorpay is called) |
| `GET /admin/finance/trial-balance`, `/journal`, `/pnl`, `/gst` | Super Admin, Admin |
| `GET /admin/dashboard`, `/admin/reports/{kind}?from&to`, `/admin/customers`, `/admin/customers/{mobile}` | all staff (read-only, from real orders) |

**Who may change what** (the same table the admin website shows, checked again on every request): products and
categories, coupons, offers, samples, reviews, care schedules and shop settings — Super Admin, Admin; stock — also
Inventory Manager; message templates — also Support; the audit log — Super Admin only. Stock counts are applied to the
live number under a row lock (`{"counted": n}`), so a count never wipes out orders placed since the screen loaded.

**Ledger** (`migrations/0005`): every money event is one balanced journal entry. The database refuses unbalanced entries and
any edit or delete of posted rows (fix mistakes with a reversing entry). Revenue is booked on delivery; money paid before that
is a customer advance (liability). The ₹5 donation is owed to the charity, not income.

| Event | Debit | Credit |
|---|---|---|
| Online payment captured | 1010 Gateway clearing | 2400 Customer advances |
| Delivered | 2400 (online) or 1000 Cash (COD) | 4000 Sales, 4100 Delivery, 4200 COD fee, 2100 GST, 2200 Donations |
| Refund before delivery | 2400 | 1010 / 1000 |
| Refund after delivery | 4500 Sales returns + 2100 GST | 1010 / 1000 |

Unpaid online orders are closed after 30 minutes (stock and the coupon use go back) by a job that runs every 5 minutes.
A payment that arrives late is never lost: it is recorded, the order is revived if the stock is still there, and otherwise
the order stays cancelled and the money is refunded automatically (if that refund fails, staff see a paid, cancelled order and
refund it from the admin). A refund Razorpay later reports as failed (`refund.failed`) is reversed in the books.

Migrations: `0005` orders, payments, ledger; `0006` the request hash behind the idempotency check; `0008` full-length stock-log ids;
`0007` the rest of the shop's catalogue (every product and coupon the website sells, so server prices match what the website shows).

**Razorpay:** set `RAZORPAY_KEY_ID`, `RAZORPAY_KEY_SECRET`, `RAZORPAY_WEBHOOK_SECRET` (test keys first — development refuses
live ones). Without them the shop offers cash on delivery only.

**Check the whole chain** (local PostgreSQL, throw-away databases — never Supabase):

```bash
DATABASE_URL=postgres://USER:PASS@localhost:5432/postgres cargo test --test db --test orders_db --test payments_db --test admin_db -- --ignored
```

It places a COD order with a coupon, checks a double tap and someone else's peek, moves it to delivered as staff, then checks
the trial balance balances, GST payable equals the order's GST, the donation is a liability, roles are enforced, and a partial
refund books a sales return.

`payments_db` covers the online-payment side: browser confirmation and its signature checks, webhooks (forged, replayed,
wrong amount), one failed attempt not closing the checkout, unpaid orders expiring, late payments (stock back, stock gone,
order already cancelled), idempotency (same key, changed request, after a cancel, two taps at once), refund limits and
races, a cancel that stands when the refund fails, a refund reversed after `refund.failed`, and the books opening for any date.

`admin_db` covers the staff side: each role limited to its own screens, bad changes refused, stock counts against live
stock, popularity counted from orders, the dashboard, reports and customers matching real orders, and no sign-in code
pretended when there is no SMS provider.

**Sign-in codes:** set `MSG91_AUTH_KEY` and `MSG91_OTP_TEMPLATE_ID` (a DLT-approved template). Without them the server
refuses to pretend a code was sent (`503 sms_unavailable`) and logs an error at start-up.

## Phone apps and own delivery riders

The Flutter apps in `../app/` use this same API. They send `X-Wagwell-Client: mobile` instead of `web` and sign in with a
**bearer token** instead of a cookie: the customer app gets its token once, as `token` in the `/auth/otp/verify` response (only
when it sends the mobile header; the website still gets an HttpOnly cookie and no token), keeps it in the phone's secure storage
and sends `Authorization: Bearer …`. A browser never attaches that header by itself, so it can't be forged cross-site.

Riders are their own accounts (`riders`, `rider_sessions`; migration `0009`), not staff: a rider can never reach the admin, and only
ever sees the orders assigned to them.

| | |
|---|---|
| `POST /api/v1/rider/auth/login` `{mobile, password}` → `{token, rider}` · `GET /rider/auth/me` · `POST /rider/auth/logout` | 14 h sessions; 5 wrong passwords lock 15 min; same rate limit as staff sign-in |
| `GET /api/v1/rider/orders` | my stops: packed, shipped or out for delivery, plus today's delivered; address with the checkout map pin, items, `collect` (cash to take, rupees) |
| `POST /rider/orders/{number}/start` | out for delivery; makes the customer's 4-digit handover code |
| `POST /rider/orders/{number}/deliver` `{code}` | the code from the customer's app; 5 wrong codes and the shop has to confirm it. Delivered = COD cash booked and the sale posted to the ledger, exactly like staff marking it delivered |
| `POST /rider/orders/{number}/attempt` `{reason}` | couldn't deliver; noted on the order |
| `POST /rider/location` `{lat, lng, accuracy?, heading?, speed?}` | latest position only (no history); deleted on sign-out or when the rider is switched off |
| `GET /api/v1/orders/{number}/tracking` | customer, own order: drop pin, and **only while out for delivery** the rider's first name, a position fresher than 10 min, and the handover code |
| `GET/POST /api/v1/admin/riders` · `POST /admin/riders/{id}/active` | list (order managers too) · add / switch off (Super Admin, Admin); switching off signs the rider out everywhere |
| `POST /api/v1/admin/orders/{number}/rider` `{riderId \| null}` | assign a rider (Super Admin, Admin, Order Manager); the admin order view shows `rider` |

First rider (or use the admin endpoint). The password goes in on standard input:

```bash
read -rs P && echo "$P" | cargo run -- create-rider 9000000001 "Ravi Kumar"
```

Check the whole delivery chain (local PostgreSQL only, never Supabase):

```bash
DATABASE_URL=postgres://USER:PASS@localhost:5432/postgres cargo test --test delivery_db -- --ignored
```

### Staff phone app: scan stock in

The staff app (`../app/admin_app`) signs in at `POST /api/v1/admin/auth/login` with `X-Wagwell-Client: mobile` and gets
`{token, admin}` back (no cookie). Its session lasts one 12-hour shift with no 30-minute idle limit (the website's sessions keep
it), because the shop phone has its own screen lock. Migration `0010` adds the session `client` column, goods-in receipts, and
batch / made-on / expiry / cost on stock log lines.

| | |
|---|---|
| `GET /api/v1/admin/barcodes/{code}` | every staff role. `{code, matches: [{product, variant, lastBatch}]}`: exact barcode first, then SKU; an unknown code is an empty list |
| `POST /api/v1/admin/inventory/receipts` `{type?, supplierRef?, note?, lines: [{variantId, qty, batchNo?, mfgDate?, expiryDate?, unitCost?}]}` | Super Admin, Admin, Inventory Manager. Needs `Idempotency-Key`: the same key returns the first result (`replayed: true`) instead of adding twice; the same key with different items is refused. Up to 200 lines, all or nothing. `type` is `addition` (default), `opening` or `return`; expired batches are refused |
| `GET /api/v1/admin/inventory/expiring?days=60` | received batches expiring within `days` (1–365) for sizes still in stock, soonest first |

Check it (local PostgreSQL only):

```bash
DATABASE_URL=postgres://USER:PASS@localhost:5432/postgres cargo test --test admin_app_db -- --ignored
```
