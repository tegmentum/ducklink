#!/usr/bin/env bash
# publish-artifacts.sh — publish ducklink deployment artifacts to R2 (get.ducklink.dev).
#
# Publishes the three downloadable deployment forms and a discoverable index:
#   * plugin     — the native DuckDB loadable extension (LOAD ducklink), one build
#                  per DuckDB version + platform
#   * standalone — the ducklink host CLI binary, one tarball per platform
#   * browser    — the @tegmentum/ducklink npm package (pure JS; wasm fetched at runtime)
#   * manifest.json + index.html — machine- and human-readable listings
#
# This is the durable, repeatable counterpart to the one-off publish. It MERGES
# into the existing manifest (fetched from R2 first), so publishing one platform
# never drops previously-published versions/platforms.
#
# URL / prefix model (shared datalink-ext bucket):
#   Objects are stored under a KEY_PREFIX (default "get/") to stay namespaced in
#   the shared bucket. Public URLs are ${DOWNLOAD_BASE}[/${URL_PREFIX}]/<form>/...
#   Defaults yield  https://get.ducklink.dev/get/plugin/...  which works the moment
#   get.ducklink.dev is added as an R2 custom domain on the datalink-ext bucket —
#   zero extra Cloudflare config.
#   For clean URLs (https://get.ducklink.dev/plugin/...): add a Cloudflare Transform
#   Rule that prepends "get/" to the path for get.ducklink.dev, then set URL_PREFIX="".
#
# Usage:
#   R2_ACCESS_KEY_ID=... R2_SECRET_ACCESS_KEY=... deploy/r2/publish-artifacts.sh
#   BUILD=1 deploy/r2/publish-artifacts.sh          # build artifacts that are missing
#   DRY_RUN=1 deploy/r2/publish-artifacts.sh        # stage + generate, do not upload
#   VERIFY=1 deploy/r2/publish-artifacts.sh         # curl-check each URL after upload
#
# Credentials (never printed): taken from env (CI org secrets). For local runs, if
# R2_ACCESS_KEY_ID is unset and ${R2_ENV_FILE:-~/git/datalink/r2.env} exists, it is
# sourced. Requires: aws CLI, python3, shasum. gzip for the plugin; cargo (BUILD);
# npm (BUILD, browser).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"

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

# --- public url model --------------------------------------------------------
DOWNLOAD_BASE="${DOWNLOAD_BASE:-https://get.ducklink.dev}"
KEY_PREFIX="${KEY_PREFIX-get}"                  # R2 key namespace in the shared bucket ("" = bucket root)
URL_PREFIX="${URL_PREFIX-$KEY_PREFIX}"          # path segment in public URLs ("" if a CF rule strips it)
key()  { local p="$1"; [ -n "$KEY_PREFIX" ] && echo "${KEY_PREFIX}/${p}" || echo "$p"; }
url()  { local p="$1"; [ -n "$URL_PREFIX" ] && echo "${DOWNLOAD_BASE}/${URL_PREFIX}/${p}" || echo "${DOWNLOAD_BASE}/${p}"; }

# --- repos + versions --------------------------------------------------------
EXT_DIR="${DUCKLINK_EXTENSION_DIR:-$HOME/git/ducklink-extension}"
JS_DIR="${DUCKLINK_JS_DIR:-$HOME/git/ducklink-js}"
HOST_DIR="${DUCKLINK_HOST_DIR:-$HOME/git/ducklink}"

# ext version from description.yml (fallback v4.0.1); DuckDB target + ABI are release facts.
EXT_VERSION="${EXT_VERSION:-$(sed -n 's/^  version: *//p' "$EXT_DIR/description.yml" 2>/dev/null | head -1)}"
EXT_VERSION="v${EXT_VERSION#v}"; [ "$EXT_VERSION" = "v" ] && EXT_VERSION="v4.0.1"
DUCKDB_TARGET="${DUCKDB_TARGET:-v1.5.4}"
EXT_ABI="${EXT_ABI:-v0.2.0}"
CLI_VERSION="${CLI_VERSION:-$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$HOST_DIR/crates/ducklink-host/Cargo.toml" 2>/dev/null | head -1)}"
CLI_VERSION="${CLI_VERSION:-0.1.0}"
BROWSER_VERSION="${BROWSER_VERSION:-$(python3 -c "import json,sys;print(json.load(open('$JS_DIR/package.json'))['version'])" 2>/dev/null || echo 0.1.0)}"

# platform tag (duckdb convention): osx_arm64 / osx_amd64 / linux_amd64 / linux_arm64
detect_platform() {
  local os arch; os="$(uname -s)"; arch="$(uname -m)"
  case "$os" in Darwin) os=osx;; Linux) os=linux;; *) os="$(echo "$os" | tr '[:upper:]' '[:lower:]')";; esac
  case "$arch" in arm64|aarch64) arch=arm64;; x86_64|amd64) arch=amd64;; esac
  echo "${os}_${arch}"
}
PLATFORM="${PLATFORM:-$(detect_platform)}"

echo "ducklink artifact publish"
echo "  bucket        r2://${R2_BUCKET}  (endpoint ${ENDPOINT})"
echo "  download base $(url '<form>/...')"
echo "  plugin        ${EXT_VERSION} for DuckDB ${DUCKDB_TARGET} / ${PLATFORM}"
echo "  standalone    ${CLI_VERSION} / ${PLATFORM}"
echo "  browser       @tegmentum/ducklink ${BROWSER_VERSION}"
[ -n "${DRY_RUN:-}" ] && echo "  (DRY_RUN — no uploads)"
echo ""

STAGE="$(mktemp -d -t ducklink-artifacts.XXXX)"
RECORDS="$STAGE/records.tsv"     # form \t version \t platform \t key \t size \t sha256 \t content_type \t extra_json
: > "$RECORDS"
trap 'rm -rf "$STAGE"' EXIT

sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }
size_of()   { stat -f%z "$1" 2>/dev/null || stat -c%s "$1"; }

# upload <file> <key> <content-type> [cache-control]
upload() {
  local file="$1" k="$2" ct="$3" cc="${4:-public, max-age=31536000, immutable}"
  if [ -n "${DRY_RUN:-}" ]; then echo "    (dry) $k"; return; fi
  AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION=auto \
    aws s3api put-object --endpoint-url "$ENDPOINT" --bucket "$R2_BUCKET" \
      --key "$k" --body "$file" --content-type "$ct" --cache-control "$cc" >/dev/null
  echo "    put $k"
}

record() { printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$@" >> "$RECORDS"; }

# --- plugin ------------------------------------------------------------------
PLUGIN_FILE="${PLUGIN_FILE:-$EXT_DIR/build/release/ducklink.duckdb_extension}"
if [ ! -f "$PLUGIN_FILE" ] && [ -n "${BUILD:-}" ]; then
  echo "building plugin (make release in $EXT_DIR)..."; ( cd "$EXT_DIR" && make release ); fi
if [ -f "$PLUGIN_FILE" ]; then
  echo "plugin:"
  gz="$STAGE/ducklink.duckdb_extension.gz"; gzip -9 -c "$PLUGIN_FILE" > "$gz"
  base="plugin/${EXT_VERSION}/${DUCKDB_TARGET}/${PLATFORM}"
  upload "$PLUGIN_FILE" "$(key "$base/ducklink.duckdb_extension")" application/octet-stream
  upload "$gz"          "$(key "$base/ducklink.duckdb_extension.gz")" application/gzip
  record plugin "$EXT_VERSION" "$PLATFORM" "$(url "$base/ducklink.duckdb_extension")" \
    "$(size_of "$PLUGIN_FILE")" "$(sha256_of "$PLUGIN_FILE")" application/octet-stream \
    "{\"duckdb_target\":\"$DUCKDB_TARGET\",\"abi\":\"$EXT_ABI\",\"gz_url\":\"$(url "$base/ducklink.duckdb_extension.gz")\",\"gz_size\":$(size_of "$gz"),\"gz_sha256\":\"$(sha256_of "$gz")\"}"
else echo "plugin: SKIP (no $PLUGIN_FILE; set PLUGIN_FILE or BUILD=1)"; fi

# --- standalone CLI ----------------------------------------------------------
CLI_TARBALL="${CLI_TARBALL:-}"
if [ -z "$CLI_TARBALL" ]; then
  bin="$HOST_DIR/target/release/ducklink"
  if [ ! -x "$bin" ] && [ -n "${BUILD:-}" ]; then
    echo "building standalone CLI (cargo build --release)..."; ( cd "$HOST_DIR" && cargo build --release -p ducklink-host --bin ducklink ); fi
  if [ -x "$bin" ]; then
    tdir="$STAGE/cli"; mkdir -p "$tdir"; cp "$bin" "$tdir/ducklink"
    CLI_TARBALL="$STAGE/ducklink-${CLI_VERSION}-${PLATFORM}.tar.gz"
    tar -czf "$CLI_TARBALL" -C "$tdir" ducklink
  fi
fi
if [ -n "$CLI_TARBALL" ] && [ -f "$CLI_TARBALL" ]; then
  echo "standalone:"
  k="standalone/${CLI_VERSION}/${PLATFORM}/$(basename "$CLI_TARBALL")"
  upload "$CLI_TARBALL" "$(key "$k")" application/gzip
  record standalone "$CLI_VERSION" "$PLATFORM" "$(url "$k")" \
    "$(size_of "$CLI_TARBALL")" "$(sha256_of "$CLI_TARBALL")" application/gzip \
    "{\"unpacked_binary\":\"ducklink\"}"
else echo "standalone: SKIP (no CLI binary/tarball; set CLI_TARBALL or BUILD=1)"; fi

# --- browser package ---------------------------------------------------------
BROWSER_TGZ="${BROWSER_TGZ:-}"
if [ -z "$BROWSER_TGZ" ] && [ -n "${BUILD:-}" ] && [ -d "$JS_DIR" ]; then
  echo "packing browser (npm pack in $JS_DIR)..."
  BROWSER_TGZ="$STAGE/$( cd "$JS_DIR" && npm pack --silent --pack-destination "$STAGE" )"
fi
if [ -n "$BROWSER_TGZ" ] && [ -f "$BROWSER_TGZ" ]; then
  echo "browser:"
  k="browser/${BROWSER_VERSION}/$(basename "$BROWSER_TGZ")"
  upload "$BROWSER_TGZ" "$(key "$k")" application/gzip
  record browser "$BROWSER_VERSION" "-" "$(url "$k")" \
    "$(size_of "$BROWSER_TGZ")" "$(sha256_of "$BROWSER_TGZ")" application/gzip \
    "{\"package\":\"@tegmentum/ducklink\"}"
else echo "browser: SKIP (no tgz; set BROWSER_TGZ or BUILD=1)"; fi

# --- fetch existing manifest (for merge) -------------------------------------
CUR_MANIFEST="$STAGE/manifest.current.json"
if [ -z "${DRY_RUN:-}" ]; then
  AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY" AWS_DEFAULT_REGION=auto \
    aws s3api get-object --endpoint-url "$ENDPOINT" --bucket "$R2_BUCKET" \
      --key "$(key manifest.json)" "$CUR_MANIFEST" >/dev/null 2>&1 || : > "$CUR_MANIFEST"
else : > "$CUR_MANIFEST"; fi

# --- generate manifest.json + index.html (merge) -----------------------------
MANIFEST="$STAGE/manifest.json"; INDEX="$STAGE/index.html"
BASE_URL="$(url '' )"; BASE_URL="${BASE_URL%/}"
PENDING_PLUGIN='linux_amd64,linux_arm64,osx_amd64,windows_amd64'
PENDING_CLI='linux_amd64,linux_arm64,osx_amd64,windows_amd64'
RECORDS="$RECORDS" CUR_MANIFEST="$CUR_MANIFEST" MANIFEST="$MANIFEST" INDEX="$INDEX" \
BASE_URL="$BASE_URL" DUCKDB_TARGET="$DUCKDB_TARGET" EXT_ABI="$EXT_ABI" \
PENDING_PLUGIN="$PENDING_PLUGIN" PENDING_CLI="$PENDING_CLI" \
python3 - <<'PY'
import json, os

def load(p):
    try:
        with open(p) as f:
            s = f.read().strip()
            return json.loads(s) if s else {}
    except Exception:
        return {}

m = load(os.environ["CUR_MANIFEST"])
m.setdefault("schema", "ducklink-artifacts/1")
m["generated"] = "auto"  # stamped by the caller/commit; kept stable to avoid churn
m["base_url"] = os.environ["BASE_URL"]
m.setdefault("note", "Downloadable ducklink deployment artifacts. The native plugin platform "
             "matrix is built by community-extensions CI; platforms published here are listed "
             "per version. See the componentized (wasm) extension catalog at "
             "https://ext.ducklink.dev/ducklink/catalog.json .")
arts = m.setdefault("artifacts", {})
arts.setdefault("plugin", {"description": "Native DuckDB loadable extension (LOAD ducklink). "
                           "One build per DuckDB version + platform.", "versions": {}})
arts.setdefault("standalone", {"description": "The ducklink host CLI binary. One tarball per platform.",
                               "versions": {}})
arts.setdefault("browser", {"description": "The @tegmentum/ducklink JS package (gen-4 browser core). "
                            "Pure JS; the wasm core + extensions are fetched at runtime.", "versions": {}})

for line in open(os.environ["RECORDS"]):
    line = line.rstrip("\n")
    if not line:
        continue
    form, version, platform, url, size, sha, ct, extra = line.split("\t")
    size = int(size); extra = json.loads(extra)
    if form == "plugin":
        v = arts["plugin"]["versions"].setdefault(version, {"platforms": {}})
        v["duckdb_target"] = extra.get("duckdb_target")
        v["extension_abi_version"] = extra.get("abi")
        v["platforms"][platform] = {
            "raw": {"url": url, "size": size, "sha256": sha, "content_type": ct},
            "gz":  {"url": extra["gz_url"], "size": extra["gz_size"], "sha256": extra["gz_sha256"],
                    "content_type": "application/gzip"},
        }
        pend = [p for p in os.environ["PENDING_PLUGIN"].split(",") if p not in v["platforms"]]
        v["pending_platforms"] = pend
    elif form == "standalone":
        v = arts["standalone"]["versions"].setdefault(version, {"platforms": {}})
        v["platforms"][platform] = {"url": url, "size": size, "sha256": sha,
                                    "content_type": ct, "unpacked_binary": extra.get("unpacked_binary", "ducklink")}
        pend = [p for p in os.environ["PENDING_CLI"].split(",") if p not in v["platforms"]]
        v["pending_platforms"] = pend
    elif form == "browser":
        arts["browser"]["versions"][version] = {"url": url, "size": size, "sha256": sha,
            "content_type": ct, "package": extra.get("package", "@tegmentum/ducklink"),
            "install": "npm install ./" + url.rsplit("/", 1)[-1]}

with open(os.environ["MANIFEST"], "w") as f:
    json.dump(m, f, indent=2); f.write("\n")

# --- human index.html ---
def mb(n): return f"{n/1_048_576:.2f} MB" if n >= 1_048_576 else f"{n/1024:.1f} KB"
rows = []
for ver, v in sorted(arts["plugin"]["versions"].items(), reverse=True):
    for plat, files in sorted(v["platforms"].items()):
        rows.append(f'<tr><td>plugin</td><td>{ver}</td><td>{plat}</td>'
                    f'<td>DuckDB {v.get("duckdb_target","")}</td>'
                    f'<td><a href="{files["raw"]["url"]}">.duckdb_extension</a> '
                    f'(<a href="{files["gz"]["url"]}">.gz</a>)</td>'
                    f'<td>{mb(files["raw"]["size"])}</td></tr>')
for ver, v in sorted(arts["standalone"]["versions"].items(), reverse=True):
    for plat, f2 in sorted(v["platforms"].items()):
        rows.append(f'<tr><td>standalone</td><td>{ver}</td><td>{plat}</td><td>CLI</td>'
                    f'<td><a href="{f2["url"]}">tar.gz</a></td><td>{mb(f2["size"])}</td></tr>')
for ver, f3 in sorted(arts["browser"]["versions"].items(), reverse=True):
    rows.append(f'<tr><td>browser</td><td>{ver}</td><td>—</td><td>npm</td>'
                f'<td><a href="{f3["url"]}">{f3["package"]} tgz</a></td><td>{mb(f3["size"])}</td></tr>')
html = ("<!doctype html><meta charset=utf-8><title>ducklink downloads</title>"
        "<style>body{font:15px/1.5 system-ui,sans-serif;max-width:60rem;margin:3rem auto;padding:0 1rem}"
        "table{border-collapse:collapse;width:100%}td,th{border-bottom:1px solid #ddd;padding:.4rem .6rem;text-align:left}"
        "code{background:#f4f4f4;padding:.1rem .3rem;border-radius:3px}</style>"
        "<h1>ducklink downloads</h1>"
        "<p>Native plugin, standalone CLI, and browser package. The plugin platform matrix is also "
        "built by <a href=https://github.com/duckdb/community-extensions>community-extensions</a>; "
        "portable wasm extension modules live in the "
        "<a href=https://ext.ducklink.dev/ducklink/catalog.json>catalog</a>. "
        "Machine-readable: <a href=manifest.json>manifest.json</a>.</p>"
        "<table><tr><th>form</th><th>version</th><th>platform</th><th>kind</th><th>download</th><th>size</th></tr>"
        + "".join(rows) + "</table>")
with open(os.environ["INDEX"], "w") as f:
    f.write(html)
print("manifest + index generated")
PY

upload "$MANIFEST" "$(key manifest.json)" application/json "public, max-age=300, must-revalidate"
upload "$INDEX"    "$(key index.html)"     "text/html; charset=utf-8" "public, max-age=300, must-revalidate"

# --- verify ------------------------------------------------------------------
if [ -n "${VERIFY:-}" ] && [ -z "${DRY_RUN:-}" ]; then
  echo "verify:"
  while IFS=$'\t' read -r form version platform u size sha ct extra; do
    code="$(curl -s -o /dev/null -w '%{http_code}' -I "$u" || echo ---)"
    echo "    $code  $u"
  done < "$RECORDS"
  mu="$(url manifest.json)"; echo "    $(curl -s -o /dev/null -w '%{http_code}' -I "$mu")  $mu"
fi

echo ""
echo "done. index: $(url index.html)"
