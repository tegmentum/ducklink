from typing import Optional, Union

import pytest

from ducklink.types import TypeMappingError, map_type


@pytest.mark.parametrize(
    "hint,expected",
    [
        (str, "VARCHAR"),
        (int, "BIGINT"),
        (float, "DOUBLE"),
        (bool, "BOOLEAN"),
        (bytes, "BLOB"),
        (bytearray, "BLOB"),
        (memoryview, "BLOB"),
    ],
)
def test_scalar_type_mapping(hint, expected):
    assert map_type(hint) == expected


@pytest.mark.parametrize(
    "hint,expected",
    [
        (Optional[str], "VARCHAR"),
        (Optional[int], "BIGINT"),
        (Union[float, None], "DOUBLE"),
        (str | None, "VARCHAR"),
        (None | bytes, "BLOB"),
    ],
)
def test_optional_maps_to_inner(hint, expected):
    assert map_type(hint) == expected


def test_unmapped_type_raises():
    with pytest.raises(TypeMappingError) as exc:
        map_type(complex)
    assert "cannot map" in str(exc.value)


def test_none_raises():
    with pytest.raises(TypeMappingError):
        map_type(None)
    with pytest.raises(TypeMappingError):
        map_type(type(None))


def test_multi_member_union_rejected():
    with pytest.raises(TypeMappingError) as exc:
        map_type(Union[str, int])
    assert "union" in str(exc.value).lower()


def test_error_includes_context():
    with pytest.raises(TypeMappingError) as exc:
        map_type(complex, context="parameter 'x'")
    assert "parameter 'x'" in str(exc.value)
