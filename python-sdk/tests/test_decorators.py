from typing import Optional

import pytest

import ducklink
from ducklink.registry import REGISTRY
from ducklink.types import TypeMappingError


def test_scalar_registers_and_stays_callable():
    @ducklink.scalar
    def add_one(x: int) -> int:
        return x + 1

    # Original function still directly callable.
    assert add_one(41) == 42

    reg = REGISTRY.get("add_one")
    assert reg.kind == "scalar"
    assert reg.arguments == [ducklink.Argument("x", "BIGINT")]
    assert reg.returns == "BIGINT"
    # entry is "module:qualname"; for a module-level def qualname == name.
    assert reg.entry.startswith("test_decorators:")
    assert reg.entry.endswith("add_one")
    # The registration keeps the original callable.
    assert reg.target is add_one


def test_scalar_bare_and_parenthesized_forms():
    @ducklink.scalar
    def a(x: str) -> str:
        return x

    @ducklink.scalar(name="renamed")
    def b(x: str) -> str:
        return x

    assert "a" in REGISTRY
    assert "renamed" in REGISTRY
    assert b("hi") == "hi"


def test_scalar_multi_arg_types():
    @ducklink.scalar
    def f(s: str, n: int, r: float, flag: bool, blob: bytes) -> str:
        return s

    reg = REGISTRY.get("f")
    assert [a.type for a in reg.arguments] == [
        "VARCHAR",
        "BIGINT",
        "DOUBLE",
        "BOOLEAN",
        "BLOB",
    ]


def test_scalar_optional_arg():
    @ducklink.scalar
    def f(s: Optional[str]) -> int:
        return len(s or "")

    assert REGISTRY.get("f").arguments == [ducklink.Argument("s", "VARCHAR")]


def test_missing_arg_hint_raises_at_decoration():
    with pytest.raises(TypeMappingError) as exc:

        @ducklink.scalar
        def f(x) -> int:  # noqa: ANN001 - intentionally unannotated
            return 1

    assert "no type hint" in str(exc.value)
    assert "f" not in REGISTRY


def test_missing_return_hint_raises():
    with pytest.raises(TypeMappingError) as exc:

        @ducklink.scalar
        def f(x: int):
            return x

    assert "return" in str(exc.value)


def test_unmapped_arg_type_raises():
    with pytest.raises(TypeMappingError):

        @ducklink.scalar
        def f(x: complex) -> int:
            return 1


def test_varargs_rejected():
    with pytest.raises(TypeMappingError) as exc:

        @ducklink.scalar
        def f(*args: int) -> int:
            return 0

    assert "args" in str(exc.value)


def test_duplicate_name_rejected():
    @ducklink.scalar
    def dup(x: int) -> int:
        return x

    with pytest.raises(ValueError):

        @ducklink.scalar(name="dup")
        def other(x: int) -> int:
            return x


def test_table_tuple_rows():
    @ducklink.table
    def rows(text: str) -> list[tuple[str, int]]:
        return [(w, len(w)) for w in text.split()]

    # Original still callable.
    assert rows("a bb") == [("a", 1), ("bb", 2)]

    reg = REGISTRY.get("rows")
    assert reg.kind == "table"
    assert reg.returns is None
    assert reg.columns == [
        ducklink.Column("c0", "VARCHAR"),
        ducklink.Column("c1", "BIGINT"),
    ]


def test_table_single_column():
    @ducklink.table
    def nums(text: str) -> list[int]:
        return [int(x) for x in text.split()]

    reg = REGISTRY.get("nums")
    assert reg.columns == [ducklink.Column("c0", "BIGINT")]


def test_table_named_columns():
    @ducklink.table(columns=["word", "length"])
    def rows(text: str) -> list[tuple[str, int]]:
        return []

    reg = REGISTRY.get("rows")
    assert reg.columns == [
        ducklink.Column("word", "VARCHAR"),
        ducklink.Column("length", "BIGINT"),
    ]


def test_table_column_count_mismatch_raises():
    with pytest.raises(TypeMappingError):

        @ducklink.table(columns=["only_one"])
        def rows(text: str) -> list[tuple[str, int]]:
            return []


def test_table_missing_return_raises():
    with pytest.raises(TypeMappingError):

        @ducklink.table
        def rows(text: str):
            return []


def test_aggregate_class():
    @ducklink.aggregate
    class Sum:
        def __init__(self) -> None:
            self.total = 0

        def step(self, value: int) -> None:
            self.total += value

        def finalize(self) -> int:
            return self.total

    # Class still usable directly.
    agg = Sum()
    agg.step(2)
    agg.step(3)
    assert agg.finalize() == 5

    reg = REGISTRY.get("Sum")
    assert reg.kind == "aggregate"
    assert reg.arguments == [ducklink.Argument("value", "BIGINT")]
    assert reg.returns == "BIGINT"
    assert reg.entry.endswith("Sum")
    assert reg.target is Sum


def test_aggregate_missing_method_raises():
    with pytest.raises(TypeMappingError) as exc:

        @ducklink.aggregate
        class Bad:
            def step(self, value: int) -> None:
                pass

    assert "finalize" in str(exc.value)


def test_aggregate_step_needs_hints():
    with pytest.raises(TypeMappingError):

        @ducklink.aggregate
        class Bad:
            def step(self, value) -> None:  # noqa: ANN001
                pass

            def finalize(self) -> int:
                return 0
