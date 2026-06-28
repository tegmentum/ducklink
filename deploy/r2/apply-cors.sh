#!/usr/bin/env bash
# apply-cors.sh — apply the R2 CORS rule to the shared extension bucket.
#
# CORS on R2 is a PERMISSION header that lets the (TBD) GCP demo origin fetch
# artifacts directly from R2 in the browser; it is NOT a proxy/hop, so the
# zero-egress property is preserved. The demo origin is the single value that
# parameterizes the rule (deploy/r2/cors.json carries a placeholder).
#
# Usage:
#   DEMO_ORIGIN="https://demo.example.dev" deploy/r2/apply-cors.sh
#
# Env:
#   DEMO_ORIGIN   the allowed browser origin (required), e.g. https://demo.ducklink.dev
#   R2_BUCKET     bucket name (default: datalink-ext)
#   R2_ACCOUNT_ID account id (default from r2.config.json)
#   R2_ACCESS_KEY_ID / R2_SECRET_ACCESS_KEY  S3 creds (required; from CI org secrets)
#
# Requires the aws CLI (works against the R2 S3 endpoint). Run from the repo root.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
: "${DEMO_ORIGIN:?set DEMO_ORIGIN to the allowed browser origin (https://...)}"
: "${R2_ACCESS_KEY_ID:?R2_ACCESS_KEY_ID not set (CI org secret)}"
: "${R2_SECRET_ACCESS_KEY:?R2_SECRET_ACCESS_KEY not set (CI org secret)}"
R2_BUCKET="${R2_BUCKET:-datalink-ext}"
R2_ACCOUNT_ID="${R2_ACCOUNT_ID:-a633389b157fd8a9ec3d3a27cd375643}"
ENDPOINT="https://${R2_ACCOUNT_ID}.r2.cloudflarestorage.com"

# Render the parameterized origin into a temp cors document.
TMP="$(mktemp -t r2-cors.XXXX.json)"
trap 'rm -f "$TMP"' EXIT
sed "s#https://DEMO_ORIGIN_PLACEHOLDER#${DEMO_ORIGIN}#" "$HERE/cors.json" > "$TMP"

echo "Applying CORS to r2://${R2_BUCKET} (origin ${DEMO_ORIGIN}) via ${ENDPOINT}"
AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" \
AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" \
AWS_DEFAULT_REGION=auto \
aws s3api put-bucket-cors \
  --endpoint-url "$ENDPOINT" \
  --bucket "$R2_BUCKET" \
  --cors-configuration "file://${TMP}"
echo "done."
