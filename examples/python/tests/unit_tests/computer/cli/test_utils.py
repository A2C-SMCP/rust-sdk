from __future__ import annotations

import pytest

from a2c_smcp.computer.cli.utils import parse_kv_pairs, resolve_import_target


class TestParseKvPairs:
    def test_none_and_empty(self) -> None:
        assert parse_kv_pairs(None) is None
        assert parse_kv_pairs("") is None
        assert parse_kv_pairs("   ") is None

    def test_basic_key_value_parsing(self) -> None:
        assert parse_kv_pairs("a:1,b:2") == {"a": "1", "b": "2"}
        assert parse_kv_pairs(" a : 1 , b : 2 ") == {"a": "1", "b": "2"}
        # ignore empty segments
        assert parse_kv_pairs("a:1, ,b:2,,") == {"a": "1", "b": "2"}

    def test_error_missing_colon_and_empty_key(self) -> None:
        with pytest.raises(ValueError):
            parse_kv_pairs("a1,b2")
        with pytest.raises(ValueError):
            parse_kv_pairs(":v")

    def test_json_object_string(self) -> None:
        # should support valid JSON object string and keep value types
        d = parse_kv_pairs('{"x": 1, "y": "2", "z": true}')
        assert d == {"x": 1, "y": "2", "z": True}

    def test_json_non_object_should_error(self) -> None:
        with pytest.raises(ValueError):
            parse_kv_pairs("[1,2,3]")
        with pytest.raises(ValueError):
            parse_kv_pairs('"str"')


class TestResolveImportTarget:
    def test_resolve_with_colon(self) -> None:
        obj = resolve_import_target("a2c_smcp.computer.cli.utils:resolve_import_target")
        assert obj is resolve_import_target

    def test_resolve_with_dot(self) -> None:
        obj = resolve_import_target("a2c_smcp.computer.cli.utils.resolve_import_target")
        assert obj is resolve_import_target

    def test_relative_import_not_allowed(self) -> None:
        with pytest.raises(ValueError):
            resolve_import_target(".cli.utils:resolve_import_target")

    def test_invalid_target_format(self) -> None:
        with pytest.raises(ValueError):
            resolve_import_target("only_module_name")

    def test_missing_attribute(self) -> None:
        with pytest.raises(AttributeError):
            resolve_import_target("a2c_smcp.computer.cli.utils:not_exist_attr")
