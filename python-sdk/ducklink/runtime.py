"""The runtime manifest surface the host consumes.

The host reads the authored functions via
``offload.run(entry="ducklink.runtime:manifest")``. :func:`manifest` returns a
JSON-serializable ``list[dict]`` describing every registered function.
"""

from __future__ import annotations

from typing import Any

from .registry import REGISTRY, Registry


def manifest(registry: Registry | None = None) -> list[dict[str, Any]]:
    """Return the JSON-serializable manifest of all registered functions.

    Each entry has the shape::

        {
          "name": str,
          "kind": "scalar" | "table" | "aggregate",
          "arguments": [{"name": str, "type": str}, ...],
          "entry": "module:callable",
          # scalar / aggregate:
          "returns": <type>,
          # table:
          "columns": [{"name": str, "type": str}, ...],
        }
    """
    reg = registry or REGISTRY
    return [r.to_manifest_entry() for r in reg.all()]
