"""The authoring decorators: ``@ducklink.scalar/table/aggregate``.

Each decorator:

1. introspects the target's type hints and maps them to DuckDB/WIT types,
2. records a :class:`~ducklink.registry.Registration` in the module-level
   registry, and
3. returns the *original* target unchanged, so it remains directly callable
   and so the host can invoke it by ``entry="module:name"``.

Failures (missing hints, unmapped types) are raised *at decoration time*.
"""

from __future__ import annotations

import inspect
import typing
from typing import Any, Callable, Optional, TypeVar, get_type_hints

from .registry import REGISTRY, Argument, Column, Registration, Registry
from .types import TypeMappingError, map_type

F = TypeVar("F", bound=Callable[..., Any])
C = TypeVar("C", bound=type)

_EMPTY = inspect.Parameter.empty


def _entry_for(target: Any) -> str:
    """Build the ``"module:qualname"`` entry string for a target."""
    module = getattr(target, "__module__", None) or "__main__"
    name = getattr(target, "__qualname__", None) or getattr(target, "__name__")
    return f"{module}:{name}"


def _resolve_hints(func: Callable[..., Any]) -> dict[str, Any]:
    """Resolve a callable's type hints, tolerating string annotations."""
    try:
        return get_type_hints(func)
    except Exception as exc:  # pragma: no cover - defensive
        raise TypeMappingError(
            f"could not resolve type hints for {func.__qualname__!r}: {exc}"
        ) from exc


def _argument_types(
    func: Callable[..., Any], *, kind: str, skip_self: bool = False
) -> list[Argument]:
    """Map a callable's parameters to a list of typed :class:`Argument`.

    Every parameter must carry a supported type hint. ``*args``/``**kwargs`` are
    rejected: the DuckDB/WIT surface has a fixed arity.
    """
    sig = inspect.signature(func)
    hints = _resolve_hints(func)
    args: list[Argument] = []

    params = list(sig.parameters.values())
    if skip_self and params:
        params = params[1:]

    for param in params:
        if param.kind in (
            inspect.Parameter.VAR_POSITIONAL,
            inspect.Parameter.VAR_KEYWORD,
        ):
            raise TypeMappingError(
                f"{func.__qualname__!r} uses *args/**kwargs; ducklink "
                f"{kind} functions must have a fixed set of typed parameters"
            )
        hint = hints.get(param.name, _EMPTY)
        if hint is _EMPTY:
            raise TypeMappingError(
                f"parameter {param.name!r} of {func.__qualname__!r} has no type "
                f"hint; every argument of a ducklink {kind} function must be "
                "annotated"
            )
        type_name = map_type(hint, context=f"parameter {param.name!r}")
        args.append(Argument(name=param.name, type=type_name))
    return args


def _return_type(func: Callable[..., Any], *, kind: str) -> str:
    hints = _resolve_hints(func)
    hint = hints.get("return", _EMPTY)
    if hint is _EMPTY:
        raise TypeMappingError(
            f"{func.__qualname__!r} has no return type hint; a ducklink {kind} "
            "function must annotate its return type"
        )
    return map_type(hint, context=f"return of {func.__qualname__!r}")


def _register(
    *,
    target: Any,
    name: str | None,
    kind: str,
    arguments: list[Argument],
    returns: str | None = None,
    columns: list[Column] | None = None,
    registry: Registry,
) -> None:
    reg = Registration(
        name=name or target.__name__,
        kind=kind,  # type: ignore[arg-type]
        arguments=arguments,
        entry=_entry_for(target),
        target=target,
        returns=returns,
        columns=columns,
    )
    registry.add(reg)


def scalar(
    _func: F | None = None,
    *,
    name: str | None = None,
    registry: Registry | None = None,
) -> Any:
    """Register a scalar function (one row in, one value out).

    Usable bare (``@ducklink.scalar``) or with args
    (``@ducklink.scalar(name="foo")``). Returns the original function.
    """
    reg = registry or REGISTRY

    def decorate(func: F) -> F:
        args = _argument_types(func, kind="scalar")
        ret = _return_type(func, kind="scalar")
        _register(
            target=func,
            name=name,
            kind="scalar",
            arguments=args,
            returns=ret,
            registry=reg,
        )
        return func

    if _func is not None:
        return decorate(_func)
    return decorate


def _columns_from_return(func: Callable[..., Any]) -> list[Column]:
    """Derive a table function's output columns from its return annotation.

    Supported return shapes:

    * ``list[tuple[T1, T2, ...]]`` / ``Iterable[tuple[...]]`` -> one column per
      tuple member, named ``c0``, ``c1``, ... .
    * ``list[T]`` / ``Iterable[T]`` of a scalar -> a single column ``c0``.

    Column names may be overridden via the decorator's ``columns=`` argument.
    """
    hints = _resolve_hints(func)
    hint = hints.get("return", _EMPTY)
    if hint is _EMPTY:
        raise TypeMappingError(
            f"{func.__qualname__!r} has no return type hint; a ducklink table "
            "function must annotate its return type (e.g. list[tuple[str, int]])"
        )

    origin = typing.get_origin(hint)
    if origin is None:
        raise TypeMappingError(
            f"return type {hint!r} of {func.__qualname__!r} is not a row "
            "collection; a table function must return list[...] / Iterable[...] "
            "of rows"
        )

    row_args = typing.get_args(hint)
    if not row_args:
        raise TypeMappingError(
            f"return type {hint!r} of {func.__qualname__!r} is missing its row "
            "element type"
        )
    row = row_args[0]

    if typing.get_origin(row) is tuple:
        members = typing.get_args(row)
        if not members or members[-1] is Ellipsis:
            raise TypeMappingError(
                f"tuple row type of {func.__qualname__!r} must have a fixed set "
                "of typed members (no ...)"
            )
        return [
            Column(name=f"c{i}", type=map_type(m, context=f"column {i}"))
            for i, m in enumerate(members)
        ]

    # Single-column table.
    return [Column(name="c0", type=map_type(row, context="column 0"))]


def table(
    _func: F | None = None,
    *,
    name: str | None = None,
    columns: list[str] | None = None,
    registry: Registry | None = None,
) -> Any:
    """Register a table function (arguments in, rows out).

    The output columns are derived from the return annotation
    (``list[tuple[...]]`` or ``list[T]``). Pass ``columns=[...]`` to name them.
    Returns the original function.
    """
    reg = registry or REGISTRY

    def decorate(func: F) -> F:
        args = _argument_types(func, kind="table")
        cols = _columns_from_return(func)
        if columns is not None:
            if len(columns) != len(cols):
                raise TypeMappingError(
                    f"columns={columns!r} has {len(columns)} names but "
                    f"{func.__qualname__!r} returns {len(cols)} columns"
                )
            cols = [Column(name=n, type=c.type) for n, c in zip(columns, cols)]
        _register(
            target=func,
            name=name,
            kind="table",
            arguments=args,
            columns=cols,
            registry=reg,
        )
        return func

    if _func is not None:
        return decorate(_func)
    return decorate


def aggregate(
    _cls: C | None = None,
    *,
    name: str | None = None,
    registry: Registry | None = None,
) -> Any:
    """Register an aggregate function, authored as a class.

    Convention: the decorated class defines
    ``__init__(self)`` (state setup), ``step(self, <typed args>)`` (fold one
    row), and ``finalize(self) -> <typed>`` (produce the result). The argument
    types come from ``step`` (minus ``self``); the return type from ``finalize``.
    Returns the original class, still directly instantiable.
    """
    reg = registry or REGISTRY

    def decorate(cls: C) -> C:
        for method in ("step", "finalize"):
            if not callable(getattr(cls, method, None)):
                raise TypeMappingError(
                    f"aggregate class {cls.__qualname__!r} must define a "
                    f"{method}() method"
                )
        args = _argument_types(cls.step, kind="aggregate", skip_self=True)
        ret = _return_type(cls.finalize, kind="aggregate")
        _register(
            target=cls,
            name=name,
            kind="aggregate",
            arguments=args,
            returns=ret,
            registry=reg,
        )
        return cls

    if _cls is not None:
        return decorate(_cls)
    return decorate
