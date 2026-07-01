# ducklink Python authoring API

Pure-Python authoring surface for the ducklink Python source tier (Phase 1a of
`docs/python-source-tier-plan.md`). Decorate scalar / table / aggregate
functions; the SDK maps Python type hints to DuckDB/WIT types and builds a
JSON-serializable manifest the host reads via
`offload.run(entry="ducklink.runtime:manifest")`.

This package is **standalone and pure-Python** — it has no wasm dependency and
is developed and tested in plain CPython. The same authored `.py` is consumed by
both the interpreted (run) and compiled execution modes.

## Install

```bash
pip install -e '.[dev]'
```

## Authoring

```python
import ducklink

@ducklink.scalar
def title_case(s: str) -> str:
    return s.title()

@ducklink.table
def words(text: str) -> list[tuple[str, int]]:
    return [(w, len(w)) for w in text.split()]

@ducklink.aggregate
class Concat:
    def __init__(self) -> None:
        self.parts: list[str] = []
    def step(self, value: str) -> None:
        self.parts.append(value)
    def finalize(self) -> str:
        return ", ".join(self.parts)

ducklink.manifest()  # -> list[dict] the host registers as SQL functions
```

Decorators **return the original callable/class**, so functions stay directly
callable (and pylon can invoke them by `entry="module:name"`).

## Type mapping

| Python hint | DuckDB/WIT |
|---|---|
| `str` | `VARCHAR` |
| `int` | `BIGINT` |
| `float` | `DOUBLE` |
| `bool` | `BOOLEAN` |
| `bytes` / `bytearray` / `memoryview` | `BLOB` |
| `Optional[T]` | same as `T` (nullable) |

Unmapped or missing hints raise `ducklink.TypeMappingError` **at decoration
time** (fail early, not at dispatch).

## Manifest shape

```json
[
  {"name": "title_case", "kind": "scalar",
   "arguments": [{"name": "s", "type": "VARCHAR"}],
   "returns": "VARCHAR", "entry": "text_tools:title_case"},
  {"name": "words", "kind": "table",
   "arguments": [{"name": "text", "type": "VARCHAR"}],
   "columns": [{"name": "c0", "type": "VARCHAR"}, {"name": "c1", "type": "BIGINT"}],
   "entry": "text_tools:words"}
]
```

## PEP 723 inline deps

```python
from ducklink import parse_dependencies_file
parse_dependencies_file("example/text_tools.py")  # -> ["unidecode>=1.3"]
```

## Tests

```bash
pytest
```
