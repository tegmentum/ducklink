#!/usr/bin/env bash
# Smoke test: the aws extension resolves AWS credentials natively on wasm and
# feeds them to httpfs (no AWS C++ SDK). Sources exercised:
#   1. environment variables (AWS_ACCESS_KEY_ID/SECRET/SESSION_TOKEN/REGION)
#   2. an INI credentials file via AWS_SHARED_CREDENTIALS_FILE (a preopened path)
#   3. a named profile from that file
# The host inherits the environment, so exported vars reach the guest's getenv.
# NB: the wasm CLI's `-c` renders only the first statement, so multi-statement
# demos are piped via stdin (which renders each).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

HOST=./target/release/ducklink
[[ -x "$HOST" ]] || cargo build --release -p duckdb-component-host --bin ducklink

echo "=== 1. env-var credential chain -> load_aws_credentials() ==="
AWS_ACCESS_KEY_ID=AKIAEXAMPLE123 AWS_SECRET_ACCESS_KEY=secretExampleKey \
AWS_SESSION_TOKEN=tok-example AWS_REGION=us-west-2 \
  "$HOST" -- duckdb-cli :memory: -c \
  "SELECT loaded_access_key_id, loaded_region FROM load_aws_credentials();"

echo "=== 2. env-var chain -> CREATE SECRET (credential_chain), show resolved secret ==="
printf "CREATE SECRET s (TYPE s3, PROVIDER credential_chain);\nSELECT secret_string FROM duckdb_secrets() WHERE name='s';\n" | \
AWS_ACCESS_KEY_ID=AKIAEXAMPLE123 AWS_SECRET_ACCESS_KEY=secretExampleKey AWS_REGION=eu-central-1 \
  "$HOST" -- duckdb-cli :memory: 2>/dev/null | grep -oE 'key_id=[^;]+;region=[^;]+'

echo "=== 3. INI credentials file (AWS_SHARED_CREDENTIALS_FILE) + named profile ==="
FIX="$ROOT/test/fixtures/aws"
mkdir -p "$FIX"
cat > "$FIX/credentials" <<'INI'
[default]
aws_access_key_id = AKIAFROMFILE000
aws_secret_access_key = fileSecretKey
region = ap-southeast-2

[work]
aws_access_key_id = AKIAWORKPROFILE
aws_secret_access_key = workSecretKey
INI
AWS_SHARED_CREDENTIALS_FILE=/aws/credentials \
  "$HOST" --dir "$FIX::/aws" -- duckdb-cli :memory: -c \
  "SELECT loaded_access_key_id, loaded_region FROM load_aws_credentials();"
AWS_SHARED_CREDENTIALS_FILE=/aws/credentials \
  "$HOST" --dir "$FIX::/aws" -- duckdb-cli :memory: -c \
  "SELECT loaded_access_key_id AS work_profile FROM load_aws_credentials('work');"

echo "=== 4. unsupported (network) provider errors clearly ==="
AWS_ACCESS_KEY_ID=x AWS_SECRET_ACCESS_KEY=y \
  "$HOST" -- duckdb-cli :memory: -c \
  "CREATE SECRET s2 (TYPE s3, PROVIDER credential_chain, CHAIN 'sso');" 2>&1 \
  | grep -oiE "'sso' credential provider is not supported on wasm" || true
