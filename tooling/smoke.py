#!/usr/bin/env python3
"""Run an extension's smoke.sql through the ducklink host CLI + assertions.

THIN DELEGATOR. The engine is the shared `datalink_tooling.smoke` (pip-from-git
dependency on tegmentum/datalink); the DB-specific behaviour (CLI argv, host bin,
prompt regex, `.mode csv` preamble, panic markers, per-extension `--build`,
network-grant env) is driven by `tooling/datalink.config.json`. This wrapper just
points the engine at that config and forwards the original CLI args. The heavy
logic now lives in datalink_tooling.

  pip install -r requirements-dev.txt   # or: pip install -e ../datalink/tooling

Usage (unchanged):
    tooling/smoke.py <name>
    tooling/smoke.py --all [-j N]
    tooling/smoke.py --build <name>
    tooling/smoke.py --list
    tooling/smoke.py --seed-expected NAME
    tooling/smoke.py --dry-run <name>
"""
from pathlib import Path

from datalink_tooling import smoke

CONFIG = str(Path(__file__).resolve().parent / "datalink.config.json")

if __name__ == "__main__":
    smoke.main(config=CONFIG)
