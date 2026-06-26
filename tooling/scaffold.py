#!/usr/bin/env python3
"""Scaffold a new DuckDB-wasm component extension.

THIN DELEGATOR. The engine is the shared `datalink_tooling.scaffold` (pip-from-git
dependency on tegmentum/datalink); the DB-specific behaviour is driven by
`tooling/datalink.config.json`. This wrapper just points the engine at that config
and forwards the original CLI args. The heavy logic now lives in datalink_tooling.

  pip install -r requirements-dev.txt   # or: pip install -e ../datalink/tooling

Usage (unchanged):
    tooling/scaffold.py <name> [--crate crate1,crate2,...] [--description "..."]
    tooling/scaffold.py --list-broken
    tooling/scaffold.py --list-worlds
    tooling/scaffold.py <name> --dry-run
"""
from pathlib import Path

from datalink_tooling import scaffold

CONFIG = str(Path(__file__).resolve().parent / "datalink.config.json")

if __name__ == "__main__":
    scaffold.main(config=CONFIG)
