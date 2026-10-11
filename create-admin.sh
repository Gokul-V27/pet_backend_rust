#!/usr/bin/env bash
# Creates a staff account in the database you choose (for example the live one on Render).
# Asks each question in plain words; the database address and password are never shown or saved.
set -euo pipefail
cd "$(dirname "$0")"

echo "Create a Wagwell staff account"
echo
echo "1) Paste the DATABASE_URL from Render (pet-backend-rust > Environment), then press Enter."
echo "   (It stays hidden while you paste.)"
read -rs -p "   Database URL: " DATABASE_URL; echo
[[ "$DATABASE_URL" == postgres* ]] || { echo "That doesn't look like a database URL (it should start with postgres). Nothing was created."; exit 1; }
export DATABASE_URL

read -r -p "2) Your email: " EMAIL
[[ "$EMAIL" == *@*.* ]] || { echo "That doesn't look like an email. Nothing was created."; exit 1; }
read -r -p "3) Your full name: " NAME
[[ -n "$NAME" ]] || { echo "Name can't be empty. Nothing was created."; exit 1; }

read -rs -p "4) New password (hidden): " P; echo
read -rs -p "   Type it again: " P2; echo
[[ "$P" == "$P2" ]] || { echo "The passwords don't match. Nothing was created."; exit 1; }
(( ${#P} >= 12 )) || { echo "Use at least 12 characters for the password. Nothing was created."; exit 1; }

echo
echo "Creating super_admin $EMAIL ..."
echo "$P" | cargo run --quiet -- create-admin "$EMAIL" "$NAME" super_admin
echo
echo "Done. Sign in at https://petadmin-beta.vercel.app/admin"
