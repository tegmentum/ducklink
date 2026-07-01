"""ducklink Python authoring API.

Author a ducklink extension in Python: decorate scalar / table / aggregate
functions, and the SDK builds a JSON-serializable manifest the host reads to
register SQL functions. The same ``.py`` runs interpreted (zero-build) or
compiled; this package is the stable authoring surface both modes consume.

Example::

    import ducklink

    @ducklink.scalar
    def title_case(s: str) -> str:
        return s.title()

The manifest is available via :func:`ducklink.runtime.manifest`, which the host
invokes through ``offload.run(entry="ducklink.runtime:manifest")``.
"""

from __future__ import annotations

from . import pep723, runtime
from .decorators import aggregate, scalar, table
from .pep723 import parse_dependencies, parse_dependencies_file
from .registry import (
    REGISTRY,
    Argument,
    Column,
    Registration,
    Registry,
)
from .runtime import manifest
from .types import TypeMappingError, map_type

__all__ = [
    "scalar",
    "table",
    "aggregate",
    "manifest",
    "runtime",
    "pep723",
    "parse_dependencies",
    "parse_dependencies_file",
    "map_type",
    "TypeMappingError",
    "REGISTRY",
    "Registry",
    "Registration",
    "Argument",
    "Column",
]

__version__ = "0.1.0"
