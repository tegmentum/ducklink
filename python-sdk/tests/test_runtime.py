import json

import ducklink
from ducklink import manifest


def _define():
    @ducklink.scalar
    def title_case(s: str) -> str:
        return s.title()

    @ducklink.table
    def words(text: str) -> list[tuple[str, int]]:
        return []

    @ducklink.aggregate
    class Concat:
        def __init__(self) -> None:
            self.parts: list[str] = []

        def step(self, value: str) -> None:
            self.parts.append(value)

        def finalize(self) -> str:
            return ", ".join(self.parts)


def test_manifest_shape():
    _define()
    m = manifest()
    assert [e["name"] for e in m] == ["title_case", "words", "Concat"]

    scalar_e = m[0]
    assert scalar_e == {
        "name": "title_case",
        "kind": "scalar",
        "arguments": [{"name": "s", "type": "VARCHAR"}],
        "entry": scalar_e["entry"],
        "returns": "VARCHAR",
    }
    assert scalar_e["entry"].endswith("title_case")

    table_e = m[1]
    assert table_e["kind"] == "table"
    assert "returns" not in table_e
    assert table_e["columns"] == [
        {"name": "c0", "type": "VARCHAR"},
        {"name": "c1", "type": "BIGINT"},
    ]

    agg_e = m[2]
    assert agg_e["kind"] == "aggregate"
    assert agg_e["returns"] == "VARCHAR"
    assert agg_e["arguments"] == [{"name": "value", "type": "VARCHAR"}]


def test_manifest_is_json_serializable():
    _define()
    s = json.dumps(manifest())
    round_tripped = json.loads(s)
    assert isinstance(round_tripped, list)
    assert all(isinstance(e, dict) for e in round_tripped)


def test_empty_manifest():
    assert manifest() == []


def test_manifest_entry_ordering_preserved():
    @ducklink.scalar
    def z(x: int) -> int:
        return x

    @ducklink.scalar
    def a(x: int) -> int:
        return x

    assert [e["name"] for e in manifest()] == ["z", "a"]
