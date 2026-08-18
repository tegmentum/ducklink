#!/usr/bin/env python3
"""Unit tests for tooling/builds.py — focused on the hash-only registration path
(register_hash_only / `record-hash`) added alongside the existing file-based path
(build_record / `record`).

Run with: pytest tooling/test_builds.py
(or python3 -m pytest tooling/test_builds.py from the repo root)
"""
import importlib.util
import json
import pathlib
import subprocess
import sys

import pytest

_HERE = pathlib.Path(__file__).resolve().parent
_SPEC = importlib.util.spec_from_file_location("builds", _HERE / "builds.py")
builds = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(builds)

VALID_HASH = "a" * 64  # syntactically valid BLAKE2b-256 hex digest for tests that
                       # don't care about a *real* content hash


@pytest.fixture
def db_paths(tmp_path, monkeypatch):
    """Point the module's BUILDS/ART/INDEX globals at a scratch directory so
    tests never touch the real registry/builds.json."""
    registry = tmp_path / "registry"
    registry.mkdir()
    builds_json = registry / "builds.json"
    art = tmp_path / "artifacts" / "extensions"
    art.mkdir(parents=True)
    index_json = registry / "index.json"
    monkeypatch.setattr(builds, "ROOT", tmp_path)
    monkeypatch.setattr(builds, "BUILDS", builds_json)
    monkeypatch.setattr(builds, "ART", art)
    monkeypatch.setattr(builds, "INDEX", index_json)
    return builds_json, art


# --------------------------------------------------------------------------
# validate_hash_hex
# --------------------------------------------------------------------------

def test_validate_hash_hex_accepts_valid_digest():
    assert builds.validate_hash_hex(VALID_HASH) == VALID_HASH


def test_validate_hash_hex_lowercases():
    assert builds.validate_hash_hex("A" * 64) == "a" * 64


@pytest.mark.parametrize("bad", [
    "",
    "deadbeef",                 # too short
    "a" * 63,                   # one char short
    "a" * 65,                   # one char long
    "g" * 64,                   # non-hex char
    "a" * 32 + " " + "a" * 31,  # whitespace embedded
    None,
    12345,
])
def test_validate_hash_hex_rejects_malformed(bad):
    with pytest.raises(ValueError):
        builds.validate_hash_hex(bad)


# --------------------------------------------------------------------------
# register_hash_only
# --------------------------------------------------------------------------

def test_register_hash_only_builds_expected_record():
    rec = builds.register_hash_only(
        "icd10-2026", "bundle", VALID_HASH, 123456, url="https://r2.example/icd10.duckdb",
        now=1000,
    )
    assert rec["name"] == "icd10-2026"
    assert rec["kind"] == "bundle"
    assert rec["core_embedded"] == []
    assert rec["created_at"] == 1000
    assert len(rec["components"]) == 1
    comp = rec["components"][0]
    assert comp["name"] == "icd10-2026"  # defaults to the record name
    assert comp["hash"] == VALID_HASH
    assert comp["size"] == 123456
    assert comp["url"] == "https://r2.example/icd10.duckdb"
    assert "artifact" not in comp
    # set_hash must match the same formula used by the file-based path
    expected = builds.set_hash([(f"component:{comp['name']}", VALID_HASH)])
    assert rec["set_hash"] == expected


def test_register_hash_only_url_is_optional():
    rec = builds.register_hash_only("no-url", "bundle", VALID_HASH, 10, now=1)
    assert "url" not in rec["components"][0]


def test_register_hash_only_component_name_override():
    rec = builds.register_hash_only(
        "icd10-2026", "bundle", VALID_HASH, 10, component_name="icd10-2026.duckdb", now=1,
    )
    assert rec["components"][0]["name"] == "icd10-2026.duckdb"


def test_register_hash_only_defaults_now_to_clock():
    rec = builds.register_hash_only("x", "bundle", VALID_HASH, 1)
    assert isinstance(rec["created_at"], int) and rec["created_at"] > 0


@pytest.mark.parametrize("bad_hash", ["short", "g" * 64, "", None])
def test_register_hash_only_rejects_bad_hash(bad_hash):
    with pytest.raises(ValueError):
        builds.register_hash_only("x", "bundle", bad_hash, 10, now=1)


@pytest.mark.parametrize("bad_size", [-1, "10", 1.5, None, True])
def test_register_hash_only_rejects_bad_size(bad_size):
    with pytest.raises(ValueError):
        builds.register_hash_only("x", "bundle", VALID_HASH, bad_size, now=1)


def test_register_hash_only_rejects_bad_kind():
    with pytest.raises(ValueError):
        builds.register_hash_only("x", "not-a-kind", VALID_HASH, 10, now=1)


def test_register_hash_only_rejects_bad_url_type():
    with pytest.raises(ValueError):
        builds.register_hash_only("x", "bundle", VALID_HASH, 10, url=123, now=1)


def test_register_hash_only_rejects_empty_name():
    with pytest.raises(ValueError):
        builds.register_hash_only("", "bundle", VALID_HASH, 10, now=1)


def test_register_hash_only_zero_size_allowed():
    rec = builds.register_hash_only("x", "bundle", VALID_HASH, 0, now=1)
    assert rec["components"][0]["size"] == 0


# --------------------------------------------------------------------------
# upsert_build + persistence round-trip (hash-only path)
# --------------------------------------------------------------------------

def test_hash_only_upsert_persists_and_round_trips(db_paths):
    builds_json, _ = db_paths
    db = builds.load_db()
    rec = builds.register_hash_only("icd10-2026", "bundle", VALID_HASH, 999, now=1)
    msg = builds.upsert_build(db, rec)
    builds.save_db(db)
    assert msg.startswith("recorded:")
    assert builds_json.exists()

    reloaded = builds.load_db()
    found = builds.find(reloaded, "icd10-2026")
    assert found is not None
    assert found["components"][0]["hash"] == VALID_HASH
    assert found["components"][0]["size"] == 999


def test_hash_only_reregister_identical_is_idempotent(db_paths):
    db = builds.load_db()
    rec1 = builds.register_hash_only("x", "bundle", VALID_HASH, 10, now=1)
    builds.upsert_build(db, rec1)
    builds.save_db(db)

    db2 = builds.load_db()
    rec2 = builds.register_hash_only("x", "bundle", VALID_HASH, 10, now=999)
    msg = builds.upsert_build(db2, rec2)
    assert msg.startswith("unchanged:")
    # created_at from the ORIGINAL record is preserved, not the re-record's `now`
    assert builds.find(db2, "x")["created_at"] == 1


def test_hash_only_reregister_conflicting_hash_exits(db_paths):
    db = builds.load_db()
    rec1 = builds.register_hash_only("x", "bundle", "a" * 64, 10, now=1)
    builds.upsert_build(db, rec1)
    builds.save_db(db)

    db2 = builds.load_db()
    rec2 = builds.register_hash_only("x", "bundle", "b" * 64, 10, now=1)
    with pytest.raises(SystemExit):
        builds.upsert_build(db2, rec2)


# --------------------------------------------------------------------------
# verify / show handle hash-only (no "artifact" key) components without crashing
# --------------------------------------------------------------------------

def test_verify_accepts_valid_hash_only_component(db_paths, capsys):
    db = builds.load_db()
    rec = builds.register_hash_only("x", "bundle", VALID_HASH, 10, now=1)
    builds.upsert_build(db, rec)
    builds.save_db(db)

    class Args:
        pass

    builds.cmd_verify(Args())
    out = capsys.readouterr().out
    assert "OK" in out


def test_verify_flags_hash_only_component_with_bad_size(db_paths, capsys):
    builds_json, _ = db_paths
    db = builds.load_db()
    rec = builds.register_hash_only("x", "bundle", VALID_HASH, 10, now=1)
    # corrupt the persisted size directly (simulating a bad hand-edit/bad caller)
    rec["components"][0]["size"] = -5
    db["builds"].append(rec)
    builds.save_db(db)

    class Args:
        pass

    with pytest.raises(SystemExit):
        builds.cmd_verify(Args())
    out = capsys.readouterr().out
    assert "missing/invalid size" in out


def test_show_handles_hash_only_component(db_paths, capsys):
    db = builds.load_db()
    rec = builds.register_hash_only(
        "x", "bundle", VALID_HASH, 10, url="https://r2.example/x", now=1,
    )
    builds.upsert_build(db, rec)
    builds.save_db(db)

    class Args:
        name = "x"

    builds.cmd_show(Args())
    out = capsys.readouterr().out
    assert "https://r2.example/x" in out


# --------------------------------------------------------------------------
# Regression: the existing file-based path is unchanged
# --------------------------------------------------------------------------

def test_file_based_record_still_requires_local_artifact(db_paths):
    with pytest.raises(SystemExit):
        builds.build_record("x", "bundle", "", ["missing@does/not/exist.wasm"], [], 1)


def test_file_based_record_still_works_with_real_file(db_paths):
    _, art = db_paths
    artifact = art / "jsonfns.wasm"
    artifact.write_bytes(b"fake-wasm-bytes")
    rec = builds.build_record("lean", "core", "core_functions,parquet",
                              ["jsonfns@artifacts/extensions/jsonfns.wasm"], [], 1)
    assert rec["components"][0]["artifact"] == "artifacts/extensions/jsonfns.wasm"
    assert rec["components"][0]["hash"] == builds.hash_file(artifact)
    assert "size" not in rec["components"][0]
    assert "url" not in rec["components"][0]


def test_file_based_and_hash_only_paths_share_upsert_conflict_rule(db_paths):
    """A name registered via the file-based path conflicts with a different
    hash-only registration under the same name, and vice versa — upsert_build
    doesn't care which path produced the record."""
    _, art = db_paths
    artifact = art / "jsonfns.wasm"
    artifact.write_bytes(b"fake-wasm-bytes")

    db = builds.load_db()
    file_rec = builds.build_record("dup", "bundle", "",
                                   ["jsonfns@artifacts/extensions/jsonfns.wasm"], [], 1)
    builds.upsert_build(db, file_rec)
    builds.save_db(db)

    db2 = builds.load_db()
    hash_rec = builds.register_hash_only("dup", "bundle", VALID_HASH, 10, now=1)
    with pytest.raises(SystemExit):
        builds.upsert_build(db2, hash_rec)


# --------------------------------------------------------------------------
# save_db writes atomically (tmpfile + rename), leaving no tmp litter behind
# --------------------------------------------------------------------------

def test_save_db_leaves_no_tmp_files(db_paths):
    builds_json, _ = db_paths
    db = builds.load_db()
    rec = builds.register_hash_only("x", "bundle", VALID_HASH, 10, now=1)
    builds.upsert_build(db, rec)
    builds.save_db(db)

    leftovers = list(builds_json.parent.glob(f".{builds_json.name}.tmp*"))
    assert leftovers == []
    assert json.loads(builds_json.read_text())["builds"][0]["name"] == "x"


# --------------------------------------------------------------------------
# CLI wiring: --help works, and an end-to-end `record-hash` invocation works
# --------------------------------------------------------------------------

def test_cli_help_runs():
    result = subprocess.run(
        [sys.executable, str(_HERE / "builds.py"), "--help"],
        capture_output=True, text=True, timeout=30,
    )
    assert result.returncode == 0
    assert "record-hash" in result.stdout


def test_cli_record_hash_help_runs():
    result = subprocess.run(
        [sys.executable, str(_HERE / "builds.py"), "record-hash", "--help"],
        capture_output=True, text=True, timeout=30,
    )
    assert result.returncode == 0
    assert "--hash" in result.stdout
    assert "--size" in result.stdout
    assert "--url" in result.stdout


def test_cli_record_hash_end_to_end(tmp_path):
    registry = tmp_path / "registry"
    registry.mkdir()
    env_root = tmp_path
    # the script computes ROOT from its own file location, not cwd, so we
    # symlink-free just run it with cwd=tmp_path is NOT enough — instead we
    # exercise the module function directly above; here we only smoke-test
    # that the subprocess CLI path parses args and reaches the hash validator
    # by intentionally passing a malformed hash and checking the error surface.
    result = subprocess.run(
        [sys.executable, str(_HERE / "builds.py"), "record-hash", "x",
         "--hash", "not-a-real-hash", "--size", "10"],
        capture_output=True, text=True, timeout=30, cwd=str(env_root),
    )
    assert result.returncode != 0
    assert "invalid hash" in (result.stdout + result.stderr)
