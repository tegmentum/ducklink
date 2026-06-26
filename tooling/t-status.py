#!/usr/bin/env python3
"""Scan lessons-learned.md for T-* item lifecycle markers; print open + closed.

THIN DELEGATOR. The engine is the shared `datalink_tooling.tstatus` (pip-from-git
dependency on tegmentum/datalink); the only DB-specific bit (the doc path) is
driven by `tooling/datalink.config.json` (feedback.lessons_doc). The `(T-N new)` /
`(T-N closed)` marker convention is shared across repos. This wrapper just points
the engine at that config and forwards the original CLI args.

  pip install -r requirements-dev.txt   # or: pip install -e ../datalink/tooling

Usage (unchanged):
  tooling/t-status.py            list all (open first, then closed)
  tooling/t-status.py open       just the open ones
  tooling/t-status.py closed     just the closed ones
"""
from pathlib import Path

from datalink_tooling import tstatus

CONFIG = str(Path(__file__).resolve().parent / "datalink.config.json")

if __name__ == "__main__":
    tstatus.main(config=CONFIG)
