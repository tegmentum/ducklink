#!/usr/bin/env python3
"""Scan lessons-learned.md for T-* item lifecycle markers and print
the open + closed lists.

Markers recognized (case-insensitive, anywhere in a line):
  (T-N new)            opened
  (T-N closed)         closed (any sub-clause: "closed inline",
                        "closed in same doc", "silently closed", ...)

The TITLE for a T-N is the first non-empty token sequence on the
SAME line after the marker  i.e. it's whatever the author wrote
next. Falls back to the surrounding markdown section title if the
marker line has no body.

Usage:
  python3 tooling/t-status.py            list all (open first, then closed)
  python3 tooling/t-status.py open       just the open ones
  python3 tooling/t-status.py closed     just the closed ones
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DOC = REPO_ROOT / "tooling" / "lessons-learned.md"

T_OPEN = re.compile(r"\(T-(\d+)\s+new\)", re.IGNORECASE)
T_CLOSED = re.compile(r"\(T-(\d+)[^)]*closed[^)]*\)", re.IGNORECASE)
SECTION = re.compile(r"^###\s+(.*?)\s*$")


def scan() -> tuple[dict[int, tuple[str, str]], dict[int, tuple[str, str]]]:
    """Return (open, closed) dicts: id  (section_title, marker_line)."""
    text = DOC.read_text()
    section_title = "(top)"
    opens: dict[int, tuple[str, str]] = {}
    closes: dict[int, tuple[str, str]] = {}
    for line in text.splitlines():
        if (m := SECTION.match(line)):
            section_title = m.group(1)
            continue
        # First-match-wins: the ORIGINAL open / close marker is the
        # canonical one; later mentions (including documentation
        # quotes of the regex patterns themselves  see T-25's own
        # body) shouldn't overwrite the recorded section title.
        for m in T_OPEN.finditer(line):
            n = int(m.group(1))
            opens.setdefault(n, (section_title, line.strip()))
        for m in T_CLOSED.finditer(line):
            n = int(m.group(1))
            closes.setdefault(n, (section_title, line.strip()))
    return opens, closes


def main() -> None:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which not in ("all", "open", "closed"):
        print(f"unknown filter: {which!r}; use one of: all open closed", file=sys.stderr)
        sys.exit(2)

    opens, closes = scan()
    # An item is OPEN if it has a new-marker AND no closed-marker.
    open_ids = sorted(set(opens) - set(closes))
    closed_ids = sorted(set(closes))

    if which in ("all", "open"):
        print(f"Open ({len(open_ids)}):")
        for n in open_ids:
            section, _ = opens[n]
            print(f"  T-{n:<3}  {section}")
        if which == "all":
            print()

    if which in ("all", "closed"):
        print(f"Closed ({len(closed_ids)}):")
        for n in closed_ids:
            section, _ = closes[n]
            print(f"  T-{n:<3}  {section}")


if __name__ == "__main__":
    main()
