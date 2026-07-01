"""The module-level registry of authored functions.

The decorators (:mod:`ducklink.decorators`) record one :class:`Registration`
per authored function/class here. :func:`ducklink.runtime.manifest` reads this
registry to produce the JSON-serializable manifest the host consumes.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Callable, Literal

Kind = Literal["scalar", "table", "aggregate"]


@dataclass(frozen=True)
class Argument:
    """A single named function argument and its DuckDB/WIT type."""

    name: str
    type: str

    def to_dict(self) -> dict[str, str]:
        return {"name": self.name, "type": self.type}


@dataclass(frozen=True)
class Column:
    """A single named output column of a table function."""

    name: str
    type: str

    def to_dict(self) -> dict[str, str]:
        return {"name": self.name, "type": self.type}


@dataclass
class Registration:
    """One authored function/class recorded in the registry.

    ``target`` is the original, still-directly-callable object (function for
    scalar/table, class for aggregate). ``entry`` is the ``"module:callable"``
    string the host passes to ``offload.run``.
    """

    name: str
    kind: Kind
    arguments: list[Argument]
    entry: str
    target: Any
    returns: str | None = None
    columns: list[Column] | None = None

    def to_manifest_entry(self) -> dict[str, Any]:
        entry: dict[str, Any] = {
            "name": self.name,
            "kind": self.kind,
            "arguments": [a.to_dict() for a in self.arguments],
            "entry": self.entry,
        }
        if self.kind == "table":
            entry["columns"] = [c.to_dict() for c in (self.columns or [])]
        else:
            entry["returns"] = self.returns
        return entry


class Registry:
    """An ordered, name-unique collection of registrations."""

    def __init__(self) -> None:
        self._by_name: dict[str, Registration] = {}
        self._order: list[str] = []

    def add(self, reg: Registration) -> None:
        if reg.name in self._by_name:
            raise ValueError(
                f"a ducklink function named {reg.name!r} is already registered; "
                "names must be unique within a module"
            )
        self._by_name[reg.name] = reg
        self._order.append(reg.name)

    def get(self, name: str) -> Registration:
        return self._by_name[name]

    def all(self) -> list[Registration]:
        return [self._by_name[n] for n in self._order]

    def clear(self) -> None:
        """Reset the registry. Primarily for tests."""
        self._by_name.clear()
        self._order.clear()

    def __len__(self) -> int:
        return len(self._order)

    def __contains__(self, name: object) -> bool:
        return name in self._by_name


# The single process-global registry the decorators write into and the runtime
# manifest reads from.
REGISTRY = Registry()
