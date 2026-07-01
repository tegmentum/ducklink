"""PEP 723 inline script metadata parsing.

Extracts the ``script`` metadata block from a ``.py`` file and returns its
declared dependencies. Later phases resolve these into a pylon ``env-id``.

Reference: https://peps.python.org/pep-0723/ — the reference regular expression
and semantics are followed exactly.
"""

from __future__ import annotations

import re
from pathlib import Path
from typing import Any

try:  # Python 3.11+ ships tomllib in the stdlib.
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - <3.11 fallback
    import tomli as tomllib  # type: ignore[no-redef]


# The reference regex from PEP 723. The ``type`` group captures the block name
# (we care about ``script``); the ``content`` group captures the raw block body.
_BLOCK_RE = re.compile(
    r"(?m)^# /// (?P<type>[a-zA-Z0-9-]+)$\s(?P<content>(^#(| .*)$\s)+)^# ///$"
)


class Pep723Error(ValueError):
    """Raised when a PEP 723 metadata block is malformed or duplicated."""


def read_metadata_block(source: str, block_type: str = "script") -> str | None:
    """Return the raw (un-prefixed) content of the named PEP 723 block.

    Returns ``None`` if no block of ``block_type`` is present. Raises
    :class:`Pep723Error` if the block appears more than once (per the spec).
    """
    matches = [
        m for m in _BLOCK_RE.finditer(source) if m.group("type") == block_type
    ]
    if len(matches) > 1:
        raise Pep723Error(
            f"multiple {block_type!r} metadata blocks found; PEP 723 permits at "
            "most one"
        )
    if not matches:
        return None

    content = matches[0].group("content")
    # Strip the leading "# " (or bare "#") from each line, per the spec.
    lines = [
        line[2:] if line.startswith("# ") else line[1:]
        for line in content.splitlines(keepends=True)
    ]
    return "".join(lines)


def parse_metadata(source: str) -> dict[str, Any]:
    """Parse the PEP 723 ``script`` block from source into a dict.

    Returns an empty dict when no ``script`` block is present.
    """
    raw = read_metadata_block(source, "script")
    if raw is None:
        return {}
    return tomllib.loads(raw)


def parse_dependencies(source: str) -> list[str]:
    """Return the ``dependencies`` list from a PEP 723 ``script`` block.

    Returns an empty list if there is no block or no ``dependencies`` key.
    """
    meta = parse_metadata(source)
    deps = meta.get("dependencies", [])
    if not isinstance(deps, list) or not all(isinstance(d, str) for d in deps):
        raise Pep723Error(
            "PEP 723 'dependencies' must be a list of requirement strings"
        )
    return deps


def parse_dependencies_file(path: str | Path) -> list[str]:
    """Read a ``.py`` file and return its PEP 723 dependency list."""
    return parse_dependencies(Path(path).read_text(encoding="utf-8"))
