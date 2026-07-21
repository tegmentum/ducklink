#!/usr/bin/env bash
# publish-native.sh — upload a native `.duckdb_extension` artifact to R2 for
# ducklink's `LOAD NATIVE 'name'` distribution.
#
# Layout (matches src/catalog.rs::NATIVE_BLOB_BASE):
#   native/sha256/<digest>/<platform>/<name>.duckdb_extension
#
# Idempotent + immutable: the digest is in the URL, so the object is either
# already correct or brand-new. Uploads set cache-control: immutable +
# content-type: application/octet-stream, so once uploaded, the object is
# safe to cache forever at the edge.
#
# Usage:
#   publish-native.sh <path> [--name <n>] [--platform <p>]
#
# Options:
#   --name <n>       Registered SQL name of the extension. Defaults to the
#                    file's basename without .duckdb_extension.
#   --platform <p>   DuckDB platform triple (osx_arm64 / linux_amd64 / ...).
#                    Auto-detected from `duckdb --version` output if omitted.
#
# Environment:
#   R2_ACCESS_KEY_ID + R2_SECRET_ACCESS_KEY — R2 API creds (loaded from
#   ${R2_ENV_FILE:-~/git/datalink/r2.env} if unset).
#
# Requires: aws CLI, shasum. No cargo/npm build — this expects the artifact to
# already exist on disk (build separately per the ducklink-native Makefile).
#
# Companion to publish-artifacts.sh (which publishes ducklink itself as a
# community-loadable plugin/CLI/browser bundle). This script is exclusively
# for the native .duckdb_extension binaries LOAD NATIVE fetches.
set -euo pipefail

usage() {
  sed -nE '/^#/{s/^# ?//; p}' "$0" | head -n 30
  exit 1
}

path=""
name=""
platform=""
while [ $# -gt 0 ]; do
  case "$1" in
    --name)     name="$2"; shift 2;;
    --platform) platform="$2"; shift 2;;
    -h|--help)  usage;;
    *)          if [ -z "$path" ]; then path="$1"; shift; else echo "unexpected arg: $1" >&2; exit 2; fi;;
  esac
done

if [ -z "$path" ]; then usage; fi
if [ ! -f "$path" ]; then echo "$path: not a file" >&2; exit 1; fi

# Default name = basename minus .duckdb_extension.
if [ -z "$name" ]; then
  base=$(basename "$path")
  name="${base%.duckdb_extension}"
fi

# Default platform via duckdb --version.
if [ -z "$platform" ]; then
  if command -v duckdb >/dev/null 2>&1; then
    platform=$(duckdb -c "PRAGMA platform;" 2>/dev/null | tail -1 | tr -d ' ' || true)
  fi
  if [ -z "$platform" ]; then
    echo "publish-native: unable to auto-detect platform. Pass --platform <triple>." >&2
    exit 1
  fi
fi

# --- credentials -------------------------------------------------------------
R2_ENV_FILE="${R2_ENV_FILE:-$HOME/git/datalink/r2.env}"
if [ -z "${R2_ACCESS_KEY_ID:-}" ] && [ -f "$R2_ENV_FILE" ]; then
  # shellcheck disable=SC1090
  set -a; source "$R2_ENV_FILE"; set +a
fi
: "${R2_ACCESS_KEY_ID:?R2_ACCESS_KEY_ID not set (CI org secret or $R2_ENV_FILE)}"
: "${R2_SECRET_ACCESS_KEY:?R2_SECRET_ACCESS_KEY not set (CI org secret or $R2_ENV_FILE)}"
R2_BUCKET="${R2_BUCKET:-datalink-ext}"
R2_ACCOUNT_ID="${R2_ACCOUNT_ID:-a633389b157fd8a9ec3d3a27cd375643}"
ENDPOINT="https://${R2_ACCOUNT_ID}.r2.cloudflarestorage.com"
export AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID"
export AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY"

digest=$(shasum -a 256 "$path" | awk '{print $1}')
key="native/sha256/${digest}/${platform}/${name}.duckdb_extension"

echo "publishing:"
echo "  local:    $path"
echo "  digest:   sha256:${digest}"
echo "  platform: ${platform}"
echo "  name:     ${name}"
echo "  bucket:   ${R2_BUCKET}"
echo "  key:      ${key}"
echo "  url:      https://ext.ducklink.dev/${key}"

if aws s3api head-object --endpoint-url "$ENDPOINT" --bucket "$R2_BUCKET" --key "$key" >/dev/null 2>&1; then
  echo "already present; nothing to upload (digest-keyed layout guarantees a no-op)"
  exit 0
fi

aws s3api put-object \
  --endpoint-url "$ENDPOINT" \
  --bucket "$R2_BUCKET" \
  --key "$key" \
  --body "$path" \
  --content-type "application/octet-stream" \
  --cache-control "public, max-age=31536000, immutable" \
  >/dev/null

# Quick verify — HEAD the object and confirm size matches.
size_local=$(wc -c < "$path" | tr -d ' ')
size_remote=$(aws s3api head-object --endpoint-url "$ENDPOINT" --bucket "$R2_BUCKET" --key "$key" --query ContentLength --output text)
if [ "$size_local" != "$size_remote" ]; then
  echo "verify: size mismatch (local=$size_local remote=$size_remote)" >&2
  exit 1
fi
echo "uploaded ($size_local bytes verified)"
