#!/usr/bin/env bash
# moodle-mcp binary installer (Arch x86_64, no Rust toolchain needed).
#   curl -fsSL https://github.com/Neudron/moodle-mcp/releases/download/v0.1.0/install.sh | bash
# Env: VER=vX.Y.Z to pick another release, INSTALL_DIR to change the target.
set -euo pipefail

REPO="Neudron/moodle-mcp"
VER="${VER:-v0.1.0}"
DEST="${INSTALL_DIR:-$HOME/.local/bin}"
ROOT="${MOODLE_ROOT:-$PWD/smx}"
URL="${MOODLE_URL:-}"
TOKEN_FILE="${MOODLE_TOKEN_FILE:-$ROOT/.moodle/token}"

# --- 1. binaries ---
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "Downloading moodle-mcp $VER..." >&2
curl -fsSL "https://github.com/$REPO/releases/download/$VER/moodle-mcp-${VER#v}-x86_64-unknown-linux-gnu.tar.gz" \
  -o "$tmp/pkg.tar.gz"
tar -xzf "$tmp/pkg.tar.gz" -C "$tmp"
mkdir -p "$DEST"
cp "$tmp/moodle-mcp" "$tmp/moodle-sync" "$DEST/"
chmod +x "$DEST/moodle-mcp" "$DEST/moodle-sync"
[ -f .env.example ] || cp "$tmp/.env.example" ./.env.example

case ":$PATH:" in
  *":$DEST:"*) ;;
  *) echo "NOTE: $DEST is not on PATH — add: export PATH=\"\$HOME/.local/bin:\$PATH\"" >&2 ;;
esac

# --- 2. credentials: ask, never invent ---
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

cat >&2 <<EOF

Done. Binaries in $DEST. Next:
  export MOODLE_ROOT="$ROOT" MOODLE_URL="$URL"
  export MOODLE_TOKEN_FILE="$TOKEN_FILE"
  $DEST/moodle-sync --all
EOF
