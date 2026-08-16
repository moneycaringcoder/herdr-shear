#!/usr/bin/env python3
"""Offline tests for shear's Herdr API contract check.

The point of these is narrow and specific: prove that the canary can fail. A
contract check that silently passes whatever it is handed is worse than no
canary at all, because it converts an unmonitored dependency into one that
looks monitored.

So the fixture here is generated from the contract constants themselves, and
every declared method, parameter, response field and enumeration member is
removed in turn and asserted to produce a non-zero exit. Adding a new entry to
the contract without the checker enforcing it makes one of these tests fail.
"""

from __future__ import annotations

import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from herdr_api_contract import (  # noqa: E402
    ENUMS,
    REQUEST_ENUMS,
    MAX_SCHEMA_BYTES,
    MIN_PROTOCOL,
    OBJECTS,
    REQUESTS,
    RESULTS,
    ContractError,
    read_schema,
    validate,
)

CHECKER = Path(__file__).with_name("herdr_api_contract.py")

# Response objects and enumerations the fixture must define because the
# contract's own objects point at them, even though nothing in this plugin
# reads them directly.
FILLER_OBJECTS = ("TabInfo",)


def request_variant(method: str) -> dict[str, Any]:
    expected = REQUESTS[method]
    properties = {
        field: {"type": "string"}
        for field in (*expected["required"], *expected["optional"])
    }
    return {
        "properties": {
            "method": {"const": method, "type": "string"},
            "params": {
                "properties": properties,
                "required": list(expected["required"]),
                "type": "object",
            },
        },
        "required": ["method", "params"],
        "type": "object",
    }


def result_variant(name: str) -> dict[str, Any]:
    properties: dict[str, Any] = {"type": {"const": name, "type": "string"}}
    for field in RESULTS[name]:
        properties[field] = {"type": "object"}
    return {
        "properties": properties,
        "required": ["type", *RESULTS[name]],
        "type": "object",
    }


def object_definition(name: str) -> dict[str, Any]:
    expected = OBJECTS[name]
    properties = {
        field: {"type": "string"}
        for field in (*expected["required"], *expected["optional"])
    }
    return {
        "properties": properties,
        "required": list(expected["required"]),
        "type": "object",
    }


def contract_schema(protocol: int = MIN_PROTOCOL) -> dict[str, Any]:
    definitions: dict[str, Any] = {
        "ResponseResult": {"oneOf": [result_variant(name) for name in sorted(RESULTS)]}
    }
    for name in OBJECTS:
        definitions[name] = object_definition(name)
    for name in ENUMS:
        definitions[name] = {"enum": list(ENUMS[name]), "type": "string"}
    for name in FILLER_OBJECTS:
        definitions.setdefault(name, {"properties": {}, "type": "object"})
    request_defs: dict[str, Any] = {
        name: {"enum": list(members), "type": "string"}
        for name, members in REQUEST_ENUMS.items()
    }
    return {
        "protocol": protocol,
        "schema_version": 1,
        "schemas": {
            "request": {
                "$defs": request_defs,
                "oneOf": [request_variant(method) for method in sorted(REQUESTS)],
            },
            "success_response": {"$defs": definitions},
        },
    }


def request_params(schema: dict[str, Any], method: str) -> dict[str, Any]:
    for variant in schema["schemas"]["request"]["oneOf"]:
        if variant["properties"]["method"]["const"] == method:
            return variant["properties"]["params"]
    raise AssertionError(f"fixture has no request variant for {method}")


def response_definitions(schema: dict[str, Any]) -> dict[str, Any]:
    return schema["schemas"]["success_response"]["$defs"]


class FixtureTests(unittest.TestCase):
    def test_the_generated_fixture_passes(self) -> None:
        self.assertEqual(validate(contract_schema()), (MIN_PROTOCOL, len(REQUESTS)))

    def test_unrelated_upstream_additions_are_tolerated(self) -> None:
        schema = contract_schema(protocol=MIN_PROTOCOL + 7)
        schema["future_root_key"] = {"anything": True}
        schema["schemas"]["request"]["oneOf"].append(
            {
                "properties": {
                    "method": {"const": "some.future.method", "type": "string"},
                    "params": {"type": "object"},
                },
                "type": "object",
            }
        )
        definitions = response_definitions(schema)
        definitions["FutureInfo"] = {"properties": {}, "type": "object"}
        for name in OBJECTS:
            definitions[name]["properties"]["future_field"] = {"type": "string"}
        for name in ENUMS:
            definitions[name]["enum"].append("future_member")
        for name in REQUEST_ENUMS:
            schema["schemas"]["request"]["$defs"][name]["enum"].append("future_member")
        self.assertEqual(validate(schema), (MIN_PROTOCOL + 7, len(REQUESTS)))


class BreakageTests(unittest.TestCase):
    """Each of these is a real upstream break this plugin would suffer."""

    def assert_rejected(self, schema: dict[str, Any], message: str) -> None:
        with self.assertRaisesRegex(ContractError, message):
            validate(schema)

    def test_an_older_protocol_is_refused(self) -> None:
        self.assert_rejected(
            contract_schema(protocol=MIN_PROTOCOL - 1), "older than the declared floor"
        )

    def test_a_removed_method_is_caught(self) -> None:
        for method in sorted(REQUESTS):
            with self.subTest(method=method):
                schema = contract_schema()
                schema["schemas"]["request"]["oneOf"] = [
                    variant
                    for variant in schema["schemas"]["request"]["oneOf"]
                    if variant["properties"]["method"]["const"] != method
                ]
                self.assert_rejected(schema, f"missing request methods: {method}")

    def test_a_renamed_method_is_caught(self) -> None:
        for method in sorted(REQUESTS):
            with self.subTest(method=method):
                schema = contract_schema()
                for variant in schema["schemas"]["request"]["oneOf"]:
                    if variant["properties"]["method"]["const"] == method:
                        variant["properties"]["method"]["const"] = f"{method}.v2"
                self.assert_rejected(schema, "missing request methods")

    def test_a_removed_parameter_is_caught(self) -> None:
        for method, expected in sorted(REQUESTS.items()):
            for field in (*expected["required"], *expected["optional"]):
                with self.subTest(method=method, field=field):
                    schema = contract_schema()
                    del request_params(schema, method)["properties"][field]
                    self.assert_rejected(schema, f"no longer accepts `{field}`")

    def test_a_parameter_that_stopped_being_required_is_caught(self) -> None:
        for method, expected in sorted(REQUESTS.items()):
            for field in expected["required"]:
                with self.subTest(method=method, field=field):
                    schema = contract_schema()
                    params = request_params(schema, method)
                    params["required"] = [
                        name for name in params["required"] if name != field
                    ]
                    self.assert_rejected(schema, f"no longer requires `{field}`")

    def test_a_removed_response_variant_is_caught(self) -> None:
        for name in sorted(RESULTS):
            with self.subTest(variant=name):
                schema = contract_schema()
                result = response_definitions(schema)["ResponseResult"]
                result["oneOf"] = [
                    variant
                    for variant in result["oneOf"]
                    if variant["properties"]["type"]["const"] != name
                ]
                self.assert_rejected(schema, f"missing response variant `{name}`")

    def test_a_removed_response_variant_field_is_caught(self) -> None:
        for name, fields in sorted(RESULTS.items()):
            for field in fields:
                with self.subTest(variant=name, field=field):
                    schema = contract_schema()
                    for variant in response_definitions(schema)["ResponseResult"]["oneOf"]:
                        if variant["properties"]["type"]["const"] == name:
                            del variant["properties"][field]
                    self.assert_rejected(schema, f"no longer carries `{field}`")

    def test_a_removed_response_object_is_caught(self) -> None:
        for name in sorted(OBJECTS):
            with self.subTest(object=name):
                schema = contract_schema()
                del response_definitions(schema)[name]
                self.assert_rejected(schema, f"missing response object `{name}`")

    def test_a_removed_response_field_is_caught(self) -> None:
        for name, expected in sorted(OBJECTS.items()):
            for field in (*expected["required"], *expected["optional"]):
                with self.subTest(object=name, field=field):
                    schema = contract_schema()
                    del response_definitions(schema)[name]["properties"][field]
                    self.assert_rejected(schema, f"`{name}` no longer carries `{field}`")

    def test_a_field_that_stopped_being_guaranteed_is_caught(self) -> None:
        for name, expected in sorted(OBJECTS.items()):
            for field in expected["required"]:
                with self.subTest(object=name, field=field):
                    schema = contract_schema()
                    definition = response_definitions(schema)[name]
                    definition["required"] = [
                        entry for entry in definition["required"] if entry != field
                    ]
                    self.assert_rejected(
                        schema, f"`{name}.{field}` is no longer always present"
                    )

    def test_a_removed_enumeration_member_is_caught(self) -> None:
        groups = (
            (ENUMS, response_definitions),
            (REQUEST_ENUMS, lambda schema: schema["schemas"]["request"]["$defs"]),
        )
        for expected, locate in groups:
            for name, members in sorted(expected.items()):
                for member in members:
                    with self.subTest(enum=name, member=member):
                        schema = contract_schema()
                        definition = locate(schema)[name]
                        definition["enum"] = [
                            entry for entry in definition["enum"] if entry != member
                        ]
                        self.assert_rejected(schema, f"no longer has `{member}`")

    def test_structural_damage_fails_closed(self) -> None:
        for mutate, message in (
            (lambda schema: schema.pop("schema_version"), "schema_version must be integer 1"),
            (lambda schema: schema.update(protocol="19"), "non-negative integer"),
            (lambda schema: schema["schemas"].pop("request"), "must be an object"),
            (
                lambda schema: schema["schemas"]["request"].update(oneOf={}),
                "oneOf must be an array",
            ),
            (
                lambda schema: schema["schemas"].pop("success_response"),
                "must be an object",
            ),
        ):
            with self.subTest(message=message):
                schema = contract_schema()
                mutate(schema)
                self.assert_rejected(schema, message)

    def test_an_unresolvable_parameter_reference_is_caught(self) -> None:
        for reference in ("https://example.invalid/params", "#/schemas/request/$defs/Gone"):
            with self.subTest(reference=reference):
                schema = contract_schema()
                method = sorted(REQUESTS)[0]
                for variant in schema["schemas"]["request"]["oneOf"]:
                    if variant["properties"]["method"]["const"] == method:
                        variant["properties"]["params"] = {"$ref": reference}
                with self.assertRaises(ContractError):
                    validate(schema)


class LocalReferenceTests(unittest.TestCase):
    """Upstream states every params schema as a `$ref`, so following one is not
    an edge case — it is the only way this check sees any parameter at all."""

    def test_parameters_behind_a_local_reference_are_followed(self) -> None:
        candidates = [
            method
            for method in sorted(REQUESTS)
            if REQUESTS[method]["required"] or REQUESTS[method]["optional"]
        ]
        if not candidates:
            self.skipTest("no method in this contract sends parameters")
        method = candidates[0]
        field = (REQUESTS[method]["required"] or REQUESTS[method]["optional"])[0]

        schema = contract_schema()
        params = copy.deepcopy(request_params(schema, method))
        schema["schemas"]["request"].setdefault("$defs", {})["Params"] = params
        for variant in schema["schemas"]["request"]["oneOf"]:
            if variant["properties"]["method"]["const"] == method:
                variant["properties"]["params"] = {
                    "$ref": "#/schemas/request/$defs/Params"
                }
        self.assertEqual(validate(schema), (MIN_PROTOCOL, len(REQUESTS)))

        del schema["schemas"]["request"]["$defs"]["Params"]["properties"][field]
        with self.assertRaisesRegex(ContractError, f"no longer accepts `{field}`"):
            validate(schema)


class InputTests(unittest.TestCase):
    def run_checker(self, content: bytes) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as directory:
            schema = Path(directory) / "schema.json"
            schema.write_bytes(content)
            return subprocess.run(
                [sys.executable, str(CHECKER), str(schema)],
                check=False,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )

    def test_a_good_schema_exits_zero_with_a_deterministic_line(self) -> None:
        result = self.run_checker(json.dumps(contract_schema()).encode())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout,
            f"Herdr API contract verified: protocol {MIN_PROTOCOL}; "
            f"{len(REQUESTS)} methods\n",
        )
        self.assertEqual(result.stderr, "")

    def test_a_broken_schema_exits_one(self) -> None:
        result = self.run_checker(json.dumps(contract_schema(protocol=1)).encode())
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("older than the declared floor", result.stderr)

    def test_malformed_input_is_rejected_without_echoing_it(self) -> None:
        for content in (
            b"{not-json secret-value",
            b'"\xffsecret-value"',
            b'{"protocol": NaN, "secret": "secret-value"}',
        ):
            with self.subTest(content=content):
                result = self.run_checker(content)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
                self.assertNotIn("secret-value", result.stderr)

    def test_an_html_error_page_is_not_mistaken_for_a_schema(self) -> None:
        # A fetch that 404s and is saved anyway must fail loudly rather than
        # quietly checking nothing.
        result = self.run_checker(b"<!DOCTYPE html><title>404</title>")
        self.assertEqual(result.returncode, 1)
        self.assertIn("not valid UTF-8 JSON", result.stderr)

    def test_an_oversized_file_is_rejected_before_parsing(self) -> None:
        result = self.run_checker(b"{" + b" " * MAX_SCHEMA_BYTES)
        self.assertEqual(result.returncode, 1)
        self.assertIn("byte limit", result.stderr)

    def test_a_directory_is_rejected_without_disclosing_its_path(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ContractError, "regular file") as raised:
                read_schema(Path(directory))
            self.assertNotIn(directory, str(raised.exception))


if __name__ == "__main__":
    unittest.main()
