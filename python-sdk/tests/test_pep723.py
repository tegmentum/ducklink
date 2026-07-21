import textwrap

import pytest

from ducklink import parse_dependencies, parse_dependencies_file
from ducklink.pep723 import Pep723Error, parse_metadata, read_metadata_block

WITH_DEPS = textwrap.dedent(
    '''\
    # /// script
    # requires-python = ">=3.11"
    # dependencies = [
    #   "requests>=2.0",
    #   "rich",
    # ]
    # ///
    """A module with inline deps."""
    x = 1
    '''
)

NO_BLOCK = textwrap.dedent(
    '''\
    """A module with no metadata block."""
    x = 1
    '''
)

BLOCK_NO_DEPS = textwrap.dedent(
    """\
    # /// script
    # requires-python = ">=3.11"
    # ///
    x = 1
    """
)

EMPTY_DEPS = textwrap.dedent(
    """\
    # /// script
    # dependencies = []
    # ///
    x = 1
    """
)


def test_parse_dependencies_with_deps():
    assert parse_dependencies(WITH_DEPS) == ["requests>=2.0", "rich"]


def test_parse_dependencies_no_block():
    assert parse_dependencies(NO_BLOCK) == []


def test_parse_dependencies_block_without_deps_key():
    assert parse_dependencies(BLOCK_NO_DEPS) == []


def test_parse_dependencies_empty_list():
    assert parse_dependencies(EMPTY_DEPS) == []


def test_parse_metadata_returns_full_dict():
    meta = parse_metadata(WITH_DEPS)
    assert meta["requires-python"] == ">=3.11"
    assert meta["dependencies"] == ["requests>=2.0", "rich"]


def test_read_metadata_block_none_when_absent():
    assert read_metadata_block(NO_BLOCK) is None


def test_duplicate_block_raises():
    dup = WITH_DEPS + "\n" + WITH_DEPS
    with pytest.raises(Pep723Error):
        parse_dependencies(dup)


def test_bad_dependencies_type_raises():
    bad = textwrap.dedent(
        """\
        # /// script
        # dependencies = "not-a-list"
        # ///
        x = 1
        """
    )
    with pytest.raises(Pep723Error):
        parse_dependencies(bad)


def test_only_script_block_parsed():
    # A non-"script" block must be ignored; the reference PEP 723 regex requires
    # blocks to be separated (not immediately adjacent) to delimit correctly.
    src = textwrap.dedent(
        """\
        # /// pyproject
        # [tool.foo]
        # bar = 1
        # ///

        x = 1

        # /// script
        # dependencies = ["a"]
        # ///
        y = 2
        """
    )
    assert parse_dependencies(src) == ["a"]


def test_parse_dependencies_file(tmp_path):
    p = tmp_path / "s.py"
    p.write_text(WITH_DEPS, encoding="utf-8")
    assert parse_dependencies_file(p) == ["requests>=2.0", "rich"]
