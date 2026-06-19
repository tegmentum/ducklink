#!/usr/bin/env python3
"""Iceberg + Avro smoke harness for the DuckDB wasm build.

Generates consistent Iceberg fixtures with pyiceberg, then asserts the iceberg /
avro surface through the native `duckdb-host` runner (the only thing that can
instantiate the core component):

  * read_avro on deflate + snappy manifests
  * iceberg_scan local (basic / snappy / partitioned)
  * gzip table metadata
  * time travel (snapshot_from_id)
  * iceberg_scan remote over HTTP (range-capable server)
  * REST catalog ATTACH: none auth, bearer token, and AWS SigV4 (signature
    independently recomputed + checked byte-for-byte)

Fixtures live under build/iceberg-fixtures/ (gitignored, regenerated on demand).
Requires: pyiceberg[snappy] + pyarrow. Test servers run in background threads.

Usage:  python3 tooling/iceberg_smoke.py [--keep-fixtures] [-v]
Exit code is non-zero if any check fails.
"""
from __future__ import annotations

import argparse
import gzip
import logging
import hashlib
import hmac
import http.server
import json
import os
import re
import shutil
import socket
import ssl
import subprocess
import sys
import threading
from pathlib import Path
from urllib.parse import urlparse

REPO_ROOT = Path(__file__).resolve().parents[1]
TARGET = REPO_ROOT / "target" / "wasm32-wasip2" / "release"
HOST_BIN = REPO_ROOT / "target" / "release" / "duckdb-host"
CORE = TARGET / "duckdb_core_component.wasm"
CLI = TARGET / "duckdb_cli_component.wasm"
FIX = REPO_ROOT / "build" / "iceberg-fixtures"
WH = FIX / "wh"

VERBOSE = False

# quiet moto/werkzeug access logging
for _n in ("werkzeug", "moto"):
    logging.getLogger(_n).setLevel(logging.ERROR)


# --- host invocation -------------------------------------------------------
def run_sql(sql: str, preopens=((FIX, FIX),), settings="", timeout=120) -> str:
    argv = [str(HOST_BIN)]
    for host_dir, guest in preopens:
        argv += ["--dir", f"{host_dir}::{guest}"]
    argv += ["--core-component", str(CORE), "--cli-component", str(CLI),
             "--", "duckdb-cli", ":memory:"]
    full = f"SET home_directory='{FIX}';\n.mode csv\n{settings}{sql}"
    out = subprocess.run(argv, input=full, capture_output=True, text=True,
                         timeout=timeout, cwd=REPO_ROOT)
    text = out.stdout + out.stderr
    if VERBOSE:
        print(f"    sql: {sql.strip()}")
        print("    out: " + text.replace("\n", "\n         "))
    return text


def cell(out: str, header: str):
    """Return the first value under a CSV column `header` in the host output."""
    lines = [re.sub(r"^(D>\s*|\.\.\.>\s*)+", "", ln).strip() for ln in out.splitlines()]
    for i, ln in enumerate(lines):
        cols = [c.strip() for c in ln.split(",")]
        if header in cols and i + 1 < len(lines):
            val = [c.strip() for c in lines[i + 1].split(",")]
            return val[cols.index(header)]
    return None


# --- fixtures --------------------------------------------------------------
def _finalize(table_dir: Path):
    """Add DuckDB-style version discovery (vN.metadata.json + version-hint)."""
    md = table_dir / "metadata"
    latest = sorted(p for p in md.glob("*.metadata.json") if not p.name.startswith("v"))[-1]
    shutil.copy2(latest, md / "v1.metadata.json")
    (md / "version-hint.text").write_text("1")


def gen_fixtures():
    import pyarrow as pa
    from pyiceberg.catalog.sql import SqlCatalog

    if FIX.exists():
        shutil.rmtree(FIX)
    WH.mkdir(parents=True)
    cat = SqlCatalog("h", uri=f"sqlite:///{FIX}/cat.db", warehouse=f"file://{WH}")
    cat.create_namespace("main")

    # basic (deflate manifests, default)
    basic = pa.table({"id": pa.array(range(20), pa.int64()),
                      "name": pa.array([f"r{i}" for i in range(20)])})
    t = cat.create_table("main.basic", schema=basic.schema)
    t.append(basic)
    _finalize(WH / "main" / "basic")

    # snappy manifests
    snap = pa.table({"id": pa.array(range(30), pa.int64())})
    t = cat.create_table("main.snap", schema=snap.schema,
                         properties={"write.avro.compression-codec": "snappy"})
    t.append(snap)
    _finalize(WH / "main" / "snap")

    # partitioned
    part = pa.table({"id": pa.array(range(40), pa.int64()),
                     "g": pa.array([i % 4 for i in range(40)], pa.int64())})
    t = cat.create_table("main.part", schema=part.schema)
    t.append(part)
    _finalize(WH / "main" / "part")

    # multi-snapshot (time travel)
    tt = cat.create_table("main.tt", schema=pa.schema([("id", pa.int64())]))
    tt.append(pa.table({"id": pa.array(range(10), pa.int64())}))
    tt.append(pa.table({"id": pa.array(range(10, 40), pa.int64())}))
    _finalize(WH / "main" / "tt")
    first_snap = list(tt.metadata.snapshots)[0].snapshot_id

    # gzip metadata variant of basic (DuckDB reads vN.gz.metadata.json)
    bmd = WH / "main" / "basic" / "metadata"
    with open(bmd / "v1.metadata.json", "rb") as f:
        (bmd / "v1.gz.metadata.json").write_bytes(gzip.compress(f.read()))

    return {"first_snap": first_snap}


def basic_manifest(table: str) -> Path:
    return sorted((WH / "main" / table / "metadata").glob("*-m0.avro"))[0]


# --- background servers ----------------------------------------------------
def _free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


class _Server:
    def __init__(self, handler, tls=False):
        self.port = _free_port()
        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", self.port), handler)
        if tls:
            ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            ctx.load_cert_chain(str(FIX / "mock.crt"), str(FIX / "mock.key"))
            self.httpd.socket = ctx.wrap_socket(self.httpd.socket, server_side=True)

    def __enter__(self):
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        return self

    def __exit__(self, *a):
        self.httpd.shutdown()


def range_handler():
    root = str(WH)

    class H(http.server.SimpleHTTPRequestHandler):
        def translate_path(self, path):
            return root + urlparse(path).path

        def do_GET(self):
            rng = self.headers.get("Range")
            path = self.translate_path(self.path)
            if rng and os.path.isfile(path):
                size = os.path.getsize(path)
                m = re.match(r"bytes=(\d*)-(\d*)", rng)
                g1, g2 = m.group(1), m.group(2)
                if g1 == "":
                    start, end = size - int(g2), size - 1
                else:
                    start, end = int(g1), (int(g2) if g2 else size - 1)
                end = min(end, size - 1)
                length = end - start + 1
                self.send_response(206)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
                self.send_header("Content-Length", str(length))
                self.send_header("Accept-Ranges", "bytes")
                self.end_headers()
                with open(path, "rb") as f:
                    f.seek(start)
                    self.wfile.write(f.read(length))
            else:
                super().do_GET()

        def log_message(self, *a):
            pass

    return H


def catalog_handler(meta_path: Path = None, token=None, sigv4_secret=None, *,
                    metadata=None, meta_location=None, table="basic", config=None):
    if metadata is None:
        metadata = json.load(open(meta_path))
    if meta_location is None:
        meta_location = "file://" + str(meta_path)
    table_cfg = config or {}

    def verify_sigv4(method, raw_path, headers, body) -> bool:
        auth = headers.get("authorization", "")
        mc = re.search(r"Credential=([^,]+)", auth)
        ms = re.search(r"SignedHeaders=([^,]+)", auth)
        mg = re.search(r"Signature=([0-9a-f]+)", auth)
        if not (mc and ms and mg):
            return False
        _, datestamp, region, service, _ = mc.group(1).split("/")
        signed_headers, their = ms.group(1), mg.group(1)
        u = urlparse(raw_path)
        payload = headers.get("x-amz-content-sha256", hashlib.sha256(body).hexdigest())
        ch = "".join(f"{h}:{headers.get(h, '')}\n" for h in signed_headers.split(";"))
        canon = "\n".join([method, u.path, u.query, ch, signed_headers, payload])
        sts = "\n".join(["AWS4-HMAC-SHA256", headers["x-amz-date"],
                         f"{datestamp}/{region}/{service}/aws4_request",
                         hashlib.sha256(canon.encode()).hexdigest()])
        hm = lambda k, m: hmac.new(k, m.encode(), hashlib.sha256).digest()
        ks = hm(hm(hm(hm(("AWS4" + sigv4_secret).encode(), datestamp), region), service), "aws4_request")
        return hmac.new(ks, sts.encode(), hashlib.sha256).hexdigest() == their

    class H(http.server.BaseHTTPRequestHandler):
        def _j(self, obj, code=200):
            b = json.dumps(obj).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)

        def do_GET(self):
            h = {k.lower(): v for k, v in self.headers.items()}
            if token is not None and not self.path.endswith("/v1/config"):
                if h.get("authorization") != f"Bearer {token}":
                    return self._j({"error": {"message": "unauthorized", "code": 401}}, 401)
            if sigv4_secret is not None and not verify_sigv4("GET", self.path, h, b""):
                return self._j({"error": {"message": "SignatureDoesNotMatch", "code": 403}}, 403)
            p = urlparse(self.path).path
            if p.endswith("/v1/config"):
                self._j({"defaults": {}, "overrides": {}})
            elif p.endswith("/v1/namespaces"):
                self._j({"namespaces": [["main"]]})
            elif p.endswith("/v1/namespaces/main/tables"):
                self._j({"identifiers": [{"namespace": ["main"], "name": table}]})
            elif p.endswith("/v1/namespaces/main/tables/" + table):
                self._j({"metadata-location": meta_location,
                         "metadata": metadata, "config": table_cfg})
            else:
                self._j({"error": {"message": "NoSuchTable", "code": 404}}, 404)

        def do_HEAD(self):
            self.send_response(200)
            self.end_headers()

        def log_message(self, *a):
            pass

    return H


def writable_catalog_handler(cat, staged):
    """REST catalog backed by a pyiceberg SqlCatalog, supporting create + commit
    so the wasm build can CREATE TABLE / INSERT (stage-create flow)."""
    from pyiceberg.schema import Schema
    from pyiceberg.table import CommitTableRequest
    from pyiceberg.exceptions import NoSuchTableError

    def mdump(md):
        return json.loads(md.model_dump_json(by_alias=True, exclude_none=True))

    def load_result(t):
        return {"metadata-location": t.metadata_location, "metadata": mdump(t.metadata), "config": {}}

    class H(http.server.BaseHTTPRequestHandler):
        def _j(self, obj, code=200):
            b = json.dumps(obj).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)

        def _err(self, msg, typ, code):
            self._j({"error": {"message": msg, "type": typ, "code": code}}, code)

        def _exists(self, t):
            try:
                cat.load_table(("main", t))
                return True
            except NoSuchTableError:
                return False

        def do_GET(self):
            p = urlparse(self.path).path
            if p.endswith("/v1/config"):
                self._j({"defaults": {}, "overrides": {}, "endpoints": [
                    "GET /v1/{prefix}/namespaces/{namespace}/tables/{table}",
                    "POST /v1/{prefix}/namespaces/{namespace}/tables",
                    "POST /v1/{prefix}/namespaces/{namespace}/tables/{table}"]})
            elif p.endswith("/v1/namespaces"):
                self._j({"namespaces": [["main"]]})
            elif p.endswith("/v1/namespaces/main/tables"):
                self._j({"identifiers": [{"namespace": ["main"], "name": n[-1]}
                                         for n in cat.list_tables("main")]})
            elif "/tables/" in p:
                t = p.rsplit("/", 1)[-1]
                try:
                    self._j(load_result(cat.load_table(("main", t))))
                except NoSuchTableError:
                    self._err("no such table", "NoSuchIcebergTableException", 404)
            else:
                self._err("not found", "NoSuchIcebergTableException", 404)

        def do_HEAD(self):
            p = urlparse(self.path).path
            ok = self._exists(p.rsplit("/", 1)[-1]) if "/tables/" in p else True
            self.send_response(200 if ok else 404)
            self.end_headers()

        def do_POST(self):
            p = urlparse(self.path).path
            n = int(self.headers.get("Content-Length", 0))
            b = self.rfile.read(n) if n else b""
            try:
                if p.endswith("/v1/namespaces/main/tables"):
                    d = json.loads(b)
                    st = cat._create_staged_table(("main", d["name"]),
                                                  Schema.model_validate(d["schema"]),
                                                  properties=d.get("properties") or {})
                    staged[d["name"]] = st
                    self._j(load_result(st))
                elif "/tables/" in p:
                    name = p.rsplit("/", 1)[-1]
                    req = CommitTableRequest.model_validate_json(b)
                    t = staged.pop(name, None) or cat.load_table(("main", name))
                    resp = cat.commit_table(t, req.requirements, req.updates)
                    self._j({"metadata-location": resp.metadata_location, "metadata": mdump(resp.metadata)})
                else:
                    self._err("bad", "BadRequest", 400)
            except Exception as e:  # noqa: BLE001
                self._err(str(e), type(e).__name__, 500)

        def log_message(self, *a):
            pass

    return H


def make_cert():
    if (FIX / "mock.crt").exists():
        return
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-keyout",
                    str(FIX / "mock.key"), "-out", str(FIX / "mock.crt"), "-days", "2",
                    "-nodes", "-subj", "/CN=127.0.0.1"], check=True, capture_output=True)


# --- checks ----------------------------------------------------------------
def check_read_avro_deflate(ctx):
    out = run_sql(f"SELECT count(*) AS n FROM read_avro('{basic_manifest('basic')}');")
    return cell(out, "n") == "1"


def check_read_avro_snappy(ctx):
    out = run_sql(f"SELECT count(*) AS n FROM read_avro('{basic_manifest('snap')}');")
    return cell(out, "n") == "1"


def check_scan_local(ctx):
    out = run_sql(f"SELECT count(*) AS n, sum(id) AS s FROM iceberg_scan('{WH}/main/basic');")
    return cell(out, "n") == "20" and cell(out, "s") == "190"


def check_scan_snappy(ctx):
    out = run_sql(f"SELECT count(*) AS n FROM iceberg_scan('{WH}/main/snap');")
    return cell(out, "n") == "30"


def check_scan_partitioned(ctx):
    out = run_sql("SELECT count(*) AS n, count(DISTINCT g) AS groups "
                  f"FROM iceberg_scan('{WH}/main/part');")
    return cell(out, "n") == "40" and cell(out, "groups") == "4"


def check_gzip_metadata(ctx):
    out = run_sql("SELECT count(*) AS n FROM iceberg_scan("
                  f"'{WH}/main/basic', metadata_compression_codec='gzip', version='1');")
    return cell(out, "n") == "20"


def check_time_travel(ctx):
    out = run_sql("SELECT count(*) AS n FROM iceberg_scan("
                  f"'{WH}/main/tt', snapshot_from_id={ctx['first_snap']});")
    return cell(out, "n") == "10"


def check_remote_http(ctx):
    with _Server(range_handler()) as srv:
        out = run_sql(
            f"SELECT count(*) AS n, sum(id) AS s FROM iceberg_scan("
            f"'http://127.0.0.1:{srv.port}/main/basic', allow_moved_paths=true);")
    return cell(out, "n") == "20" and cell(out, "s") == "190"


def _meta(table):
    return WH / "main" / table / "metadata" / "v1.metadata.json"


def check_catalog_none(ctx):
    with _Server(catalog_handler(_meta("basic"))) as srv:
        out = run_sql(
            f"ATTACH 'wh' AS lake (TYPE ICEBERG, ENDPOINT 'http://127.0.0.1:{srv.port}', "
            "AUTHORIZATION_TYPE 'none');\n"
            "SELECT count(*) AS n, sum(id) AS s FROM lake.main.basic;")
    return cell(out, "n") == "20" and cell(out, "s") == "190"


def check_catalog_bearer(ctx):
    with _Server(catalog_handler(_meta("basic"), token="tok-abc")) as srv:
        out = run_sql(
            "CREATE SECRET ic (TYPE ICEBERG, TOKEN 'tok-abc');\n"
            f"ATTACH 'wh' AS lake (TYPE ICEBERG, ENDPOINT 'http://127.0.0.1:{srv.port}', SECRET ic);\n"
            "SELECT count(*) AS n FROM lake.main.basic;")
    return cell(out, "n") == "20"


def check_catalog_sigv4(ctx):
    make_cert()
    secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
    with _Server(catalog_handler(_meta("basic"), sigv4_secret=secret), tls=True) as srv:
        out = run_sql(
            f"CREATE SECRET s3 (TYPE S3, KEY_ID 'AKIDEXAMPLE', SECRET '{secret}', REGION 'us-east-1');\n"
            f"ATTACH 'wh' AS lake (TYPE ICEBERG, ENDPOINT '127.0.0.1:{srv.port}', "
            "AUTHORIZATION_TYPE 'sigv4', SECRET s3);\n"
            "SELECT count(*) AS n FROM lake.main.basic;",
            settings="SET enable_curl_server_cert_verification=false;\n")
    return cell(out, "n") == "20"


def check_writes(ctx):
    """CREATE TABLE + INSERT through an attached writable catalog, then read the
    committed data back in a fresh ATTACH (proves the catalog commit persisted)."""
    try:
        from pyiceberg.catalog.sql import SqlCatalog
    except ImportError:
        return None
    wdir = FIX / "writewh"
    wdir.mkdir(exist_ok=True)
    cat = SqlCatalog("wr", uri=f"sqlite:///{FIX}/writecat.db", warehouse=f"file://{wdir}")
    cat.create_namespace("main")
    with _Server(writable_catalog_handler(cat, {})) as srv:
        ep = f"http://127.0.0.1:{srv.port}"
        run_sql(
            f"ATTACH 'wh' AS lake (TYPE ICEBERG, ENDPOINT '{ep}', AUTHORIZATION_TYPE 'none', "
            "SUPPORT_STAGE_CREATE true);\n"
            "CREATE TABLE lake.main.t (id INTEGER, v VARCHAR);\n"
            "INSERT INTO lake.main.t SELECT range, 'r' || range FROM range(25);\n"
            "INSERT INTO lake.main.t VALUES (99, 'last');")
        out = run_sql(
            f"ATTACH 'wh' AS lake (TYPE ICEBERG, ENDPOINT '{ep}', AUTHORIZATION_TYPE 'none');\n"
            "SELECT count(*) AS n, max(id) AS mx FROM lake.main.t;")
    return cell(out, "n") == "26" and cell(out, "mx") == "99"


def check_vended_credentials(ctx):
    """REST catalog vends S3 creds + endpoint in LoadTable config; data lives on
    a moto S3. With NO global S3 settings, a successful read proves the vended
    config was applied. Skipped (returns None) if moto/boto3 are not installed."""
    try:
        from moto.server import ThreadedMotoServer
        import boto3
        import pyarrow as pa
        import pyarrow.fs as pafs
        from pyiceberg.catalog.sql import SqlCatalog
    except ImportError:
        return None  # skip

    key, sec = "AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
    mp = _free_port()
    moto = ThreadedMotoServer(ip_address="127.0.0.1", port=mp)
    moto.start()
    try:
        ep = f"http://127.0.0.1:{mp}"
        boto3.client("s3", endpoint_url=ep, aws_access_key_id=key, aws_secret_access_key=sec,
                     region_name="us-east-1").create_bucket(Bucket="icebucket")
        cat = SqlCatalog("v", uri=f"sqlite:///{FIX}/moto_cat.db", warehouse="s3://icebucket/wh",
                         **{"s3.endpoint": ep, "s3.access-key-id": key, "s3.secret-access-key": sec,
                            "s3.region": "us-east-1"})
        cat.create_namespace("main")
        data = pa.table({"id": pa.array(range(50), pa.int64()),
                         "v": pa.array([i * 3 for i in range(50)], pa.int64())})
        t = cat.create_table("main.s3tbl", schema=data.schema)
        t.append(data)
        s3fs = pafs.S3FileSystem(endpoint_override=ep, access_key=key, secret_key=sec, scheme="http")
        with s3fs.open_input_stream(t.metadata_location.replace("s3://", "")) as f:
            meta = json.loads(f.read())
        config = {"s3.access-key-id": key, "s3.secret-access-key": sec, "s3.region": "us-east-1",
                  "s3.endpoint": ep, "s3.path-style-access": "true"}
        handler = catalog_handler(metadata=meta, meta_location=t.metadata_location,
                                  table="s3tbl", config=config)
        with _Server(handler) as srv:
            out = run_sql(
                f"ATTACH 'wh' AS lake (TYPE ICEBERG, ENDPOINT 'http://127.0.0.1:{srv.port}', "
                "AUTHORIZATION_TYPE 'none');\n"
                "SELECT count(*) AS n, sum(v) AS s FROM lake.main.s3tbl;")
        return cell(out, "n") == "50" and cell(out, "s") == "3675"
    finally:
        moto.stop()


CHECKS = [
    ("read_avro deflate", check_read_avro_deflate),
    ("read_avro snappy", check_read_avro_snappy),
    ("iceberg_scan local", check_scan_local),
    ("iceberg_scan snappy", check_scan_snappy),
    ("iceberg_scan partitioned", check_scan_partitioned),
    ("gzip metadata", check_gzip_metadata),
    ("time travel", check_time_travel),
    ("remote http (range)", check_remote_http),
    ("REST catalog: none", check_catalog_none),
    ("REST catalog: bearer", check_catalog_bearer),
    ("REST catalog: sigv4", check_catalog_sigv4),
    ("vended credentials (S3)", check_vended_credentials),
    ("writes (CREATE + INSERT)", check_writes),
]


def main():
    global VERBOSE
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep-fixtures", action="store_true")
    ap.add_argument("-v", "--verbose", action="store_true")
    args = ap.parse_args()
    VERBOSE = args.verbose

    for need in (HOST_BIN, CORE, CLI):
        if not need.exists():
            print(f"missing {need.relative_to(REPO_ROOT)} — run: make host && make core")
            return 2
    try:
        import pyiceberg  # noqa: F401
        import pyarrow  # noqa: F401
    except ImportError:
        print("missing deps — run: python3 -m pip install 'pyiceberg[snappy]' pyarrow")
        return 2

    print("generating fixtures…")
    ctx = gen_fixtures()

    failures = skipped = 0
    for name, fn in CHECKS:
        try:
            ok = fn(ctx)
        except Exception as e:  # noqa: BLE001
            ok, name = False, f"{name} (exception: {e})"
        if ok is None:
            print(f"  SKIP  {name} (install moto[server] + boto3 to enable)")
            skipped += 1
            continue
        print(f"  {'PASS' if ok else 'FAIL'}  {name}")
        failures += not ok

    if not args.keep_fixtures:
        shutil.rmtree(FIX, ignore_errors=True)
    ran = len(CHECKS) - skipped
    extra = f" ({skipped} skipped)" if skipped else ""
    print(f"\n{ran - failures}/{ran} checks passed{extra}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
