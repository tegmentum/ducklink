# sync-catalog-snapshot.sh — sync the bundled ducklink-extension catalog snapshot into
# the live R2 catalog that browser/CLI extensions fetch at runtime.
#
# Source of truth (bundled): ducklink-extension/assets/catalog-snapshot.json
# Live target (fetched by users): R2 key ducklink/catalog.json in the datalink-ext
# bucket, served publicly at https://ext.ducklink.dev/catalog.json.
#
# Why MERGE and not overwrite:
#   The live catalog carries top-level fields the bundled snapshot may not
#   (or may lag on): _schema (documentation of field meanings), builtins
#   (statically-linked DuckDB extensions), dotcmds, categories, registry_url,
#   version. These are edited on the live side between releases. Blowing them
#   away with the bundle's copy would drop human-curated docs and metadata.
#
#   Instead: fetch the live JSON, REPLACE the extensions array with the bundle's
#   extensions array (the only thing the release actually re-derives), bump the
#   `updated` timestamp to now, and put the result back. Every OTHER top-level
#   key on the live catalog is preserved byte-for-byte.
#
# Backup policy:
#   Before the put, the current live object is copied to
#     ducklink/catalog.backup.<UTC-timestamp>.json
#   inside the same bucket. Cheap, immutable, easy to grep for a rollback.
#
# When to use this:
#   After cutting a ducklink-extension release that regenerates
#   assets/catalog-snapshot.json — run this with --dry-run first, review the
#   additions/removals summary, then rerun without --dry-run to mirror.
#
# Usage:
#   publish-catalog.sh [--snapshot <path>] [--dry-run]
#
# Options:
#   --snapshot <path>  Local snapshot to publish. Defaults to
#                      $HOME/git/ducklink-extension/assets/catalog-snapshot.json.
#   --dry-run          Fetch + merge + diff, but do NOT back up or upload.
#
# Environment:
#   R2_ACCESS_KEY_ID + R2_SECRET_ACCESS_KEY — R2 API creds (loaded from
#   ${R2_ENV_FILE:-~/git/datalink/r2.env} if unset).
#   R2_BUCKET      — override the target bucket (default datalink-ext).
#   R2_ACCOUNT_ID  — override the R2 account (default the tegmentum account).
#
# Requires: aws CLI, python3, md5sum (or md5 on macOS).
#
# Companion to publish-native.sh (native .duckdb_extension binaries) and
# publish-artifacts.sh (plugin/standalone/browser deploy artifacts). Those
# publish immutable, digest-keyed objects; this one mutates a single mutable
# JSON file, so it does a backup + merge instead of a plain put.
set -euo pipefail

# --- args --------------------------------------------------------------------
DEFAULT_SNAPSHOT="$HOME/git/ducklink-extension/assets/catalog-snapshot.json"
snapshot="$DEFAULT_SNAPSHOT"
dry_run=0

usage() {
  sed -nE '/^#/{s/^# ?//; p}' "$0" | head -n 45
  exit 1
}

while [ $# -gt 0 ]; do
  case "$1" in
    --snapshot) snapshot="$2"; shift 2;;
    --dry-run)  dry_run=1; shift;;
    -h|--help)  usage;;
    *) echo "unexpected arg: $1" >&2; exit 2;;
  esac
done

if [ ! -f "$snapshot" ]; then
  echo "publish-catalog: snapshot not found: $snapshot" >&2
  exit 1
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
export AWS_DEFAULT_REGION="${AWS_DEFAULT_REGION:-auto}"

KEY="ducklink/catalog.json"
NOW_TS="$(date -u +%Y%m%dT%H%M%SZ)"
BACKUP_KEY="ducklink/catalog.backup.${NOW_TS}.json"

STAGE="$(mktemp -d -t ducklink-catalog.XXXX)"
trap 'rm -rf "$STAGE"' EXIT
LIVE="$STAGE/live.json"
MERGED="$STAGE/merged.json"

echo "publish-catalog:"
echo "  snapshot: $snapshot"
echo "  bucket:   ${R2_BUCKET}"
echo "  key:      ${KEY}"
echo "  backup:   ${BACKUP_KEY}"
echo "  endpoint: ${ENDPOINT}"
[ "$dry_run" = "1" ] && echo "  (dry-run — no backup, no upload)"
echo ""

# --- fetch live --------------------------------------------------------------
if ! aws s3api get-object --endpoint-url "$ENDPOINT" --bucket "$R2_BUCKET" --key "$KEY" "$LIVE" >/dev/null; then
  echo "publish-catalog: unable to fetch current live catalog r2://${R2_BUCKET}/${KEY}" >&2
  exit 1
fi
live_size=$(wc -c < "$LIVE" | tr -d ' ')
echo "fetched live: $live_size bytes"

# --- merge + diff summary ----------------------------------------------------
SNAPSHOT="$snapshot" LIVE="$LIVE" MERGED="$MERGED" NOW_ISO="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
python3 - <<'PY'
import json, os, sys

with open(os.environ["LIVE"]) as f:
    live = json.load(f)
with open(os.environ["SNAPSHOT"]) as f:
    snap = json.load(f)

live_exts = live.get("extensions") or []
snap_exts = snap.get("extensions") or []
if not isinstance(snap_exts, list):
    print("publish-catalog: snapshot .extensions is not a list", file=sys.stderr)
    sys.exit(1)

def names(exts):
    return [e.get("name") for e in exts if isinstance(e, dict) and e.get("name")]

live_names = names(live_exts)
snap_names = names(snap_exts)
live_set = set(live_names)
snap_set = set(snap_names)
additions = sorted(snap_set - live_set)
removals  = sorted(live_set - snap_set)
unchanged = sorted(live_set & snap_set)

# Detect in-place modifications (same name, different content).
by_name_live = {e.get("name"): e for e in live_exts if isinstance(e, dict) and e.get("name")}
by_name_snap = {e.get("name"): e for e in snap_exts if isinstance(e, dict) and e.get("name")}
modifications = []
for n in unchanged:
    if by_name_live[n] != by_name_snap[n]:
        modifications.append(n)
modifications.sort()

# Build merged catalog: live is the base, extensions is replaced wholesale,
# updated is bumped to now. Every other top-level key comes from live.
merged = dict(live)
merged["extensions"] = snap_exts
merged["updated"] = os.environ["NOW_ISO"]

with open(os.environ["MERGED"], "w") as f:
    # ensure_ascii=True keeps the file byte-identical across platforms and
    # matches how the live catalog is encoded today; sort_keys=False preserves
    # the top-level order the live catalog already uses.
    json.dump(merged, f, ensure_ascii=True, indent=2, sort_keys=False)
    f.write("\n")

# --- diff summary ---
def sample(xs, n=25):
    if len(xs) <= n:
        return xs
    return xs[:n] + [f"... (+{len(xs) - n} more)"]

print("diff:")
print(f"  extensions live -> snapshot : {len(live_exts)} -> {len(snap_exts)}")
print(f"  additions ({len(additions)}):")
for n in sample(additions):
    print(f"    + {n}")
print(f"  removals ({len(removals)}):")
for n in sample(removals):
    print(f"    - {n}")
print(f"  modified (same name, different fields) ({len(modifications)}):")
for n in sample(modifications):
    print(f"    ~ {n}")
PY

merged_size=$(wc -c < "$MERGED" | tr -d ' ')
echo ""
echo "merged: $merged_size bytes"

# md5 of merged file — R2 returns this as the ETag for single-part uploads,
# so we can HEAD-verify after the put.
if command -v md5sum >/dev/null 2>&1; then
  merged_md5=$(md5sum "$MERGED" | awk '{print $1}')
else
  merged_md5=$(md5 -q "$MERGED")
fi
echo "  md5:  $merged_md5"

if [ "$dry_run" = "1" ]; then
  echo ""
  echo "dry-run: skipping backup + upload. merged JSON left at:"
  echo "  $MERGED"
  # keep the merged file around for inspection when dry-running
  trap - EXIT
  exit 0
fi

# --- backup live -------------------------------------------------------------
echo ""
echo "backup: copying r2://${R2_BUCKET}/${KEY} -> r2://${R2_BUCKET}/${BACKUP_KEY}"
aws s3api copy-object \
  --endpoint-url "$ENDPOINT" \
  --bucket "$R2_BUCKET" \
  --copy-source "${R2_BUCKET}/${KEY}" \
  --key "$BACKUP_KEY" \
  --metadata-directive COPY \
  >/dev/null

# --- upload merged -----------------------------------------------------------
echo "upload: putting merged catalog to r2://${R2_BUCKET}/${KEY}"
aws s3api put-object \
  --endpoint-url "$ENDPOINT" \
  --bucket "$R2_BUCKET" \
  --key "$KEY" \
  --body "$MERGED" \
  --content-type "application/json" \
  --cache-control "public, max-age=300" \
  >/dev/null

# --- verify at R2 origin -----------------------------------------------------
# HEAD directly at the R2 endpoint — the CDN (ext.ducklink.dev) will lag ~5 min
# behind due to Cache-Control: max-age=300, so verifying the CDN would be flaky.
head_out=$(aws s3api head-object --endpoint-url "$ENDPOINT" --bucket "$R2_BUCKET" --key "$KEY")
remote_size=$(printf '%s' "$head_out" | python3 -c 'import json,sys;print(json.load(sys.stdin)["ContentLength"])')
remote_etag=$(printf '%s' "$head_out" | python3 -c 'import json,sys;print(json.load(sys.stdin)["ETag"].strip(chr(34)))')

echo "verify (R2 origin):"
echo "  size:   local=$merged_size remote=$remote_size"
echo "  etag:   local(md5)=$merged_md5 remote=$remote_etag"

fail=0
if [ "$merged_size" != "$remote_size" ]; then
  echo "  FAIL: size mismatch" >&2; fail=1
fi
if [ "$merged_md5" != "$remote_etag" ]; then
  echo "  FAIL: etag mismatch (upload may have been multipart; check aws s3api version)" >&2; fail=1
fi
if [ "$fail" != "0" ]; then exit 1; fi

echo "  OK"
echo ""
echo "done. public url: https://ext.ducklink.dev/catalog.json (CDN cache TTL 300s)"
echo "backup preserved at r2://${R2_BUCKET}/${BACKUP_KEY}"
