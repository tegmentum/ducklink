#!/usr/bin/env bash
# publish-catalog.sh -- publish the ducklink content-addressed catalog to R2.
#
# Uploads, ADDITIVELY + IDEMPOTENTLY (old blobs are never deleted and a blob
# already present at its content-addressed key is HEAD-skipped, so a mid-publish
# read never breaks and re-runs are cheap):
#   * each extension blob  -> wasm/sha256/<content_digest>/<name>.wasm
#                             (application/wasm, immutable -- content-addressed)
#   * the catalog          -> ducklink/catalog.json   (== registry/index.json)
#                             (application/json, short cache) -- uploaded LAST so
#                             the swap to the new digests is atomic.
#
# The blob key is content-addressed by the registry's `content_digest`, which is
# sha256 of the deployed .wasm. Build deterministically first
# (scripts/det-build.sh --all) and re-stamp the registry so digests match bytes.
#
# Credentials come from r2.env (R2_ACCESS_KEY_ID / R2_SECRET_ACCESS_KEY /
# R2_ACCOUNT_ID / R2_BUCKET); never printed. Default location: ../datalink/r2.env.
#
# Usage:
#   deploy/r2/publish-catalog.sh                 # upload blobs + catalog
#   R2_ENV=/path/to/r2.env deploy/r2/publish-catalog.sh
#   DRY_RUN=1 deploy/r2/publish-catalog.sh       # list what would upload
set -euo pipefail

HERE="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$HERE"

R2_ENV="${R2_ENV:-$HERE/../datalink/r2.env}"
[ -f "$R2_ENV" ] || { echo "error: r2.env not found at $R2_ENV (set R2_ENV)" >&2; exit 1; }
# shellcheck disable=SC1090
set -a; . "$R2_ENV"; set +a

: "${R2_ACCESS_KEY_ID:?R2_ACCESS_KEY_ID not set in r2.env}"
: "${R2_SECRET_ACCESS_KEY:?R2_SECRET_ACCESS_KEY not set in r2.env}"
R2_ACCOUNT_ID="${R2_ACCOUNT_ID:-a633389b157fd8a9ec3d3a27cd375643}"
R2_BUCKET="${R2_BUCKET:-datalink-ext}"
ENDPOINT="https://${R2_ACCOUNT_ID}.r2.cloudflarestorage.com"

DRY_RUN="${DRY_RUN:-}"

export AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID"
export AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY"
export AWS_DEFAULT_REGION=auto

head_exists() { # <key> -> 0 if object exists
  aws s3api head-object --endpoint-url "$ENDPOINT" --bucket "$R2_BUCKET" \
    --key "$1" >/dev/null 2>&1
}

s3put() { # <local> <key> <content-type> <cache-control>
  local src="$1" key="$2" ctype="$3" cc="$4"
  if [ -n "$DRY_RUN" ]; then echo "  [dry-run] PUT $key  <= $src ($ctype; $cc)"; return; fi
  aws s3api put-object \
    --endpoint-url "$ENDPOINT" \
    --bucket "$R2_BUCKET" \
    --key "$key" \
    --body "$src" \
    --content-type "$ctype" \
    --cache-control "$cc" >/dev/null
}

echo "Publishing ducklink catalog to r2://${R2_BUCKET} via ${ENDPOINT}${DRY_RUN:+ (dry-run)}"

# 1) blobs: wasm/sha256/<digest>/<name>.wasm  (additive + HEAD-skip idempotent)
mapfile -t ROWS < <(
  python3 - <<'PY'
import json
reg = json.load(open("registry/index.json"))
for e in reg["extensions"]:
    cd = e.get("content_digest")
    if cd:
        print(f"{e['name']}\t{cd}")
PY
)

n_put=0; n_skip=0; n_keep=0
missing=(); mism=()
for row in "${ROWS[@]}"; do
  name="${row%%$'\t'*}"; digest="${row##*$'\t'}"
  key="wasm/sha256/${digest}/${name}.wasm"
  src="artifacts/extensions/${name}.wasm"
  if [ ! -f "$src" ]; then
    # No local artifact (e.g. sqlitewasm, kept on its existing blob). It MUST
    # already exist in the bucket at its content-addressed key.
    if head_exists "$key"; then n_keep=$((n_keep+1)); else missing+=("$name"); fi
    continue
  fi
  actual="$(shasum -a 256 "$src" | awk '{print $1}')"
  if [ "$actual" != "$digest" ]; then
    echo "  MISMATCH $name: artifact sha256 $actual != registry $digest" >&2
    mism+=("$name"); continue
  fi
  if head_exists "$key"; then
    n_skip=$((n_skip+1))
  else
    s3put "$src" "$key" "application/wasm" "public, max-age=31536000, immutable"
    n_put=$((n_put+1))
  fi
done

echo "blobs: $n_put uploaded, $n_skip already-present (skipped), $n_keep kept-existing (no local artifact)"

# 2) catalog: ducklink/catalog.json (== registry/index.json) -- LAST (atomic swap)
s3put "registry/index.json" "ducklink/catalog.json" "application/json" "public, max-age=60"
echo "published ducklink/catalog.json (atomic swap to new digests)"

fail=0
if [ "${#mism[@]}" -ne 0 ]; then echo "WARNING: digest mismatch (skipped): ${mism[*]}" >&2; fail=1; fi
if [ "${#missing[@]}" -ne 0 ]; then echo "ERROR: no local artifact AND no existing blob: ${missing[*]}" >&2; fail=1; fi
exit "$fail"
