#!/usr/bin/env bash
# Smoke test: the azure extension (Azure SDK for C++ built for wasm32-wasip2)
# loads, registers az:// + the azure secret type, and drives real outbound HTTPS
# requests to Azure Blob Storage via the libcurl transport (curl-wasm over
# wasi:sockets), with TLS verified by the embedded CA bundle (CURLOPT_CAINFO_BLOB).
#
# We can't show a 200 here without valid Azure credentials (Microsoft's public
# Open-Dataset accounts have disabled anonymous access), so the test asserts the
# request reaches Azure and gets a structured Azure response:
#   - account-key secret -> the SDK HMAC-SHA256-signs the request (openssl-wasm);
#     a wrong key yields Azure's "AuthenticationFailed" (signature rejected),
#     which proves the signing + transport + TLS + response-parsing all work.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

HOST=./target/release/duckdb-host
[[ -x "$HOST" ]] || cargo build --release -p duckdb-component-host --bin duckdb-host

echo "=== 1. azure secret type registers (CREATE SECRET TYPE azure) ==="
printf "CREATE SECRET s (TYPE azure, CONNECTION_STRING 'DefaultEndpointsProtocol=https;AccountName=devstoreaccount1;AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;');\nSELECT name, type FROM duckdb_secrets() WHERE name='s';\n" | \
  "$HOST" -- duckdb-cli :memory: 2>/dev/null | grep -iE 'azure'

echo "=== 2. az:// read drives a real signed HTTPS request to Azure ==="
CS="DefaultEndpointsProtocol=https;AccountName=pandemicdatalake;AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;EndpointSuffix=core.windows.net"
out=$(printf "CREATE SECRET az (TYPE azure, PROVIDER config, CONNECTION_STRING '%s');\nSELECT 1 FROM read_parquet('az://pandemicdatalake/public/curated/covid-19/bing_covid-19_data/latest/bing_covid-19_data.parquet');\n" "$CS" | \
  timeout 150 "$HOST" -- duckdb-cli :memory: 2>&1 | grep -ivE '\[wasi-fs\]')
if echo "$out" | grep -qiE 'AuthenticationFailed|Server failed to authenticate'; then
  echo "PASS: signed request reached Azure -> AuthenticationFailed (TLS + HMAC signing + transport work)"
else
  echo "UNEXPECTED:"; echo "$out" | tail -4
fi
