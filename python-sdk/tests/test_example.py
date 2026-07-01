import importlib.util
import json
from pathlib import Path

import pytest

from ducklink import parse_dependencies_file
from ducklink.registry import REGISTRY

EXAMPLE = Path(__file__).resolve().parent.parent / "example" / "text_tools.py"


@pytest.fixture()
def text_tools():
    """Import the example module fresh against the clean registry."""
    spec = importlib.util.spec_from_file_location("text_tools", EXAMPLE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_example_manifest_reads_correctly(text_tools):
    import ducklink

    m = ducklink.manifest()
    by_name = {e["name"]: e for e in m}
    assert set(by_name) == {"title_case", "shout", "maybe_len", "words", "Concat"}

    assert by_name["title_case"]["kind"] == "scalar"
    assert by_name["title_case"]["returns"] == "VARCHAR"
    assert by_name["title_case"]["entry"] == "text_tools:title_case"

    assert by_name["shout"]["arguments"] == [
        {"name": "s", "type": "VARCHAR"},
        {"name": "times", "type": "BIGINT"},
    ]

    assert by_name["maybe_len"]["arguments"] == [{"name": "s", "type": "VARCHAR"}]
    assert by_name["maybe_len"]["returns"] == "BIGINT"

    assert by_name["words"]["kind"] == "table"
    assert by_name["words"]["columns"] == [
        {"name": "c0", "type": "VARCHAR"},
        {"name": "c1", "type": "BIGINT"},
    ]

    assert by_name["Concat"]["kind"] == "aggregate"
    assert by_name["Concat"]["returns"] == "VARCHAR"

    # Manifest is JSON-serializable.
    json.dumps(m)


def test_example_functions_still_callable(text_tools):
    assert text_tools.title_case("hello world") == "Hello World"
    assert text_tools.words("a bb ccc") == [("a", 1), ("bb", 2), ("ccc", 3)]

    c = text_tools.Concat()
    c.step("a")
    c.step("b")
    assert c.finalize() == "a, b"


def test_example_pep723_deps():
    assert parse_dependencies_file(EXAMPLE) == ["unidecode>=1.3"]
