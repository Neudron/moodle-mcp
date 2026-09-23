#!/usr/bin/env bash
# moodle-mcp bootstrap: builds both binaries and collects the credentials
# the project needs. The token is written straight to the token FILE
# (mode 600) — it never goes into env, history, or logs.
set -euo pipefail

ROOT="${MOODLE_ROOT:-$PWD/smx}"
URL="${MOODLE_URL:-}"
TOKEN_FILE="${MOODLE_TOKEN_FILE:-$ROOT/.moodle/token}"

# --- 1. credentials: ask, never invent ---
if [ -z "$URL" ]; then
  printf 'Moodle base URL (e.g. https://your-school.edu/moodle): ' >&2
  read -r URL
fi
URL="${URL%/}"
mkdir -p "$ROOT/.moodle"
chmod 700 "$ROOT/.moodle"

if [ ! -s "$TOKEN_FILE" ]; then
  echo "Paste your Moodle webservice token (profile -> Security keys)." >&2
  echo "It will be written to $TOKEN_FILE with mode 600." >&2
  printf 'Token: ' >&2
  read -rs TOKEN
  echo >&2
  printf '%s' "$TOKEN" > "$TOKEN_FILE"
  unset TOKEN
fi
chmod 600 "$TOKEN_FILE"

# --- 2. build ---
cargo build --release
BIN="$PWD/target/release"

cat >&2 <<EOF

Done. Next:
  export MOODLE_ROOT="$ROOT" MOODLE_URL="$URL"
  export MOODLE_TOKEN_FILE="$TOKEN_FILE"
  $BIN/moodle-sync --all
  MCP stdio server: $BIN/moodle-mcp
EOF
