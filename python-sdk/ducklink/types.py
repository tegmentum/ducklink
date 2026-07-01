"""Python type hint -> DuckDB/WIT type-name mapping.

The mapping is deliberately small and explicit. The goal is a stable, obvious
surface: a hint the SDK understands maps to a single canonical DuckDB/WIT type
name; anything else fails *at decoration time* with a clear message, so authors
never discover a bad signature at dispatch time.
"""

from __future__ import annotations

import types
import typing
from typing import Any


def _is_union_origin(origin: Any) -> bool:
    """True for both ``typing.Union[...]`` and PEP 604 ``X | Y`` origins."""
    return origin is typing.Union or origin is getattr(types, "UnionType", ())


class TypeMappingError(TypeError):
    """Raised when a Python type hint cannot be mapped to a DuckDB/WIT type.

    Raised eagerly (at decoration time) so signature mistakes surface early.
    """


# Canonical Python-type -> DuckDB/WIT type-name table. Order does not matter;
# lookups are exact by the concrete type object.
_SCALAR_MAP: dict[type, str] = {
    str: "VARCHAR",
    int: "BIGINT",
    float: "DOUBLE",
    bool: "BOOLEAN",
    bytes: "BLOB",
    bytearray: "BLOB",
    memoryview: "BLOB",
}


def _is_optional(hint: Any) -> tuple[bool, Any]:
    """Return (is_optional, inner_hint).

    Recognises ``Optional[T]`` / ``Union[T, None]`` / ``T | None``. If optional,
    ``inner_hint`` is the non-``None`` member (only single-member optionals are
    supported; a genuine multi-type union is rejected by the caller).
    """
    origin = typing.get_origin(hint)
    # Handle both typing.Union[T, None] (incl. Optional[T]) and PEP 604
    # (T | None), whose origin is types.UnionType on 3.10+.
    if _is_union_origin(origin):
        args = typing.get_args(hint)
        non_none = [a for a in args if a is not type(None)]
        has_none = len(non_none) != len(args)
        if has_none and len(non_none) == 1:
            return True, non_none[0]
        # Union with >1 real member, or no None: not a supported optional.
        return False, hint
    return False, hint


def map_type(hint: Any, *, context: str = "") -> str:
    """Map a single Python type hint to a DuckDB/WIT type name.

    ``Optional[T]`` maps to the same type name as ``T`` (nullability is implicit
    in DuckDB columns). Anything unmapped raises :class:`TypeMappingError`.
    """
    where = f" for {context}" if context else ""

    if hint is None or hint is type(None):
        raise TypeMappingError(
            f"bare None / NoneType is not a valid type{where}; "
            "annotate a concrete type such as str, int, float, bool, or bytes"
        )

    is_opt, inner = _is_optional(hint)
    if is_opt:
        return map_type(inner, context=context)

    # Reject unsupported unions explicitly (distinct from Optional).
    if _is_union_origin(typing.get_origin(hint)):
        raise TypeMappingError(
            f"unsupported union type {hint!r}{where}; "
            "only Optional[T] (a union with None) is supported"
        )

    mapped = _SCALAR_MAP.get(hint)
    if mapped is None:
        supported = ", ".join(
            sorted({t.__name__ for t in _SCALAR_MAP})
        )
        raise TypeMappingError(
            f"cannot map Python type {getattr(hint, '__name__', hint)!r}{where} "
            f"to a DuckDB/WIT type; supported types are: {supported} "
            "(each optionally wrapped in Optional[...])"
        )
    return mapped
