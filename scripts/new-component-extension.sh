#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 ]]; then
  cat <<'USAGE' >&2
Usage: scripts/new-component-extension.sh <extension-name>

Creates a new componentized extension crate under extensions/<name>-component
based on the sample template.
USAGE
  exit 1
fi

EXT_NAME="$1"
PACKAGE_NAME="${EXT_NAME}-component"
TEMPLATE_DIR="extensions/sample-extension-component"
DEST_DIR="extensions/${PACKAGE_NAME}"

if [[ ! -d "$TEMPLATE_DIR" ]]; then
  echo "Template directory '$TEMPLATE_DIR' not found" >&2
  exit 1
fi

if [[ -e "$DEST_DIR" ]]; then
  echo "Destination '$DEST_DIR' already exists, refusing to overwrite" >&2
  exit 1
fi

cp -R "$TEMPLATE_DIR" "$DEST_DIR"

perl -0pi -e "s/sample-extension-component/${PACKAGE_NAME}/g" "$DEST_DIR/Cargo.toml"

cat <<NOTICE
Created ${DEST_DIR}.
Next steps:
  1. Add "extensions/${PACKAGE_NAME}" to the [workspace].members list in Cargo.toml.
  2. Edit ${DEST_DIR}/src/lib.rs with your extension logic.
  3. Build it with 'cargo component build -p ${PACKAGE_NAME} --target wasm32-wasip2 --release'.
NOTICE
