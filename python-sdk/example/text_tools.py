# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "unidecode>=1.3",
# ]
# ///
"""An example ducklink extension authored in Python.

Run interpreted or compiled off this same file. ``manifest()`` describes every
function below for the host to register as SQL functions.
"""

from __future__ import annotations

from typing import Optional

import ducklink


@ducklink.scalar
def title_case(s: str) -> str:
    """Title-case a string."""
    return s.title()


@ducklink.scalar
def shout(s: str, times: int) -> str:
    """Uppercase and repeat a string."""
    return (s.upper() + " ") * times


@ducklink.scalar
def maybe_len(s: Optional[str]) -> int:
    """Length of a string, or 0 when NULL."""
    return len(s) if s is not None else 0


@ducklink.table
def words(text: str) -> list[tuple[str, int]]:
    """Split ``text`` into (word, length) rows."""
    return [(w, len(w)) for w in text.split()]


@ducklink.aggregate
class Concat:
    """Concatenate string inputs with a separator."""

    def __init__(self) -> None:
        self.parts: list[str] = []

    def step(self, value: str) -> None:
        self.parts.append(value)

    def finalize(self) -> str:
        return ", ".join(self.parts)


if __name__ == "__main__":
    import json

    print(json.dumps(ducklink.manifest(), indent=2))
