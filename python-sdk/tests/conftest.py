import pytest

from ducklink.registry import REGISTRY


@pytest.fixture(autouse=True)
def _clean_registry():
    """Isolate every test: start and end with an empty global registry."""
    REGISTRY.clear()
    yield
    REGISTRY.clear()
