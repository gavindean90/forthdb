from __future__ import annotations

import argparse
import json
import tempfile
import unittest
from pathlib import Path
from typing import Any, Mapping

from deployment_demo import run_deployment as run_kernel_deployment
from forthdb_kernel import (
    EntityId,
    Fact,
    ForthDB,
    Literal,
    Pattern,
    Predicate,
    SlotId,
    Symbol,
    Variable,
)
from library_demo import run_library as run_kernel_library
from world_deployment_demo import run_deployment as run_world_deployment
from world_library_demo import run_library as run_world_library


ROOT = Path(__file__).resolve().parent
CONFORMANCE_V1 = ROOT / "conformance" / "v1"


class ConformanceFailure(AssertionError):
    pass


def _load_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ConformanceFailure(f"{path} must contain a JSON object")
    return value


def _canonical(value: Any) -> Any:
    if isinstance(value, list):
        normalized = [_canonical(item) for item in value]
        if all(isinstance(item, dict) for item in normalized):
            return sorted(
                normalized,
                key=lambda item: json.dumps(item, sort_keys=True, separators=(",", ":")),
            )
        return normalized
    if isinstance(value, dict):
        return {key: _canonical(value[key]) for key in sorted(value)}
    return value


def _require_equal(name: str, actual: Any, expected: Any) -> None:
    actual_canonical = _canonical(actual)
    expected_canonical = _canonical(expected)
    if actual_canonical != expected_canonical:
        raise ConformanceFailure(
            f"{name} mismatch\n"
            f"expected:\n{json.dumps(expected_canonical, indent=2, sort_keys=True)}\n"
            f"actual:\n{json.dumps(actual_canonical, indent=2, sort_keys=True)}"
        )


def _atom(spec: Mapping[str, Any], entities: Mapping[str, EntityId]):
    if set(spec) == {"entity"}:
        return entities[str(spec["entity"])]
    if set(spec) == {"literal"}:
        return Literal(str(spec["literal"]))
    raise ConformanceFailure(f"Invalid atom: {spec!r}")


def _term(spec: Mapping[str, Any], entities: Mapping[str, EntityId]):
    if set(spec) == {"variable"}:
        return Variable(str(spec["variable"]))
    return _atom(spec, entities)


def _source_term(spec: Mapping[str, Any], entities: Mapping[str, EntityId]):
    if set(spec) == {"symbol"}:
        return Symbol(str(spec["symbol"]))
    return _term(spec, entities)


def _fact(spec: Mapping[str, Any], entities: Mapping[str, EntityId]) -> Fact:
    return Fact(
        _atom(spec["subject"], entities),
        Predicate(str(spec["predicate"])),
        _atom(spec["object"], entities),
    )


def _pattern(spec: Mapping[str, Any], entities: Mapping[str, EntityId]) -> Pattern:
    return Pattern(
        _term(spec["subject"], entities),
        Predicate(str(spec["predicate"])),
        _term(spec["object"], entities),
    )


def _normalize_value(value: Any, reverse_entities: Mapping[EntityId, str]) -> dict[str, str]:
    if isinstance(value, EntityId):
        try:
            return {"entity": reverse_entities[value]}
        except KeyError as exc:
            raise ConformanceFailure(f"Unknown entity in result: {value}") from exc
    if isinstance(value, Literal):
        return {"literal": value.value}
    if isinstance(value, Predicate):
        return {"predicate": value.name}
    raise ConformanceFailure(f"Unsupported result value: {value!r}")


def _normalize_fact(fact: Fact | None, reverse_entities: Mapping[EntityId, str]) -> Any:
    if fact is None:
        return None
    return {
        "subject": _normalize_value(fact.subject, reverse_entities),
        "predicate": fact.predicate.name,
        "object": _normalize_value(fact.object, reverse_entities),
    }


def _normalize_rows(result, reverse_entities: Mapping[EntityId, str], include_provenance: bool):
    rows: list[dict[str, Any]] = []
    for row in result.rows:
        normalized = {
            "binding": {
                name: _normalize_value(value, reverse_entities)
                for name, value in sorted(row.binding.items())
            }
        }
        if include_provenance:
            normalized["provenance"] = [slot.value for slot in row.provenance]
        rows.append(normalized)
    return _canonical(rows)


def _run_kernel_case(case: Mapping[str, Any]) -> dict[str, Any]:
    db = ForthDB()
    entities: dict[str, EntityId] = {}
    reverse_entities: dict[EntityId, str] = {}
    compiled: dict[str, Pattern] = {}
    assertions: list[dict[str, Any]] = []

    for name in case.get("entities", []):
        entity = db.entity()
        entities[str(name)] = entity
        reverse_entities[entity] = str(name)

    for index, step in enumerate(case.get("steps", []), start=1):
        op = str(step["op"])
        label = str(step.get("name", f"step-{index}"))

        if op == "define":
            db.define(SlotId(str(step["slot"])), _fact(step["fact"], entities))
            continue
        if op == "forget":
            db.forget(SlotId(str(step["slot"])))
            continue
        if op == "display_name":
            db.define_display_name(entities[str(step["entity"])], str(step["value"]))
            continue
        if op == "bind_symbol":
            db.bind_symbol(
                str(step["namespace"]),
                Symbol(str(step["symbol"])),
                entities[str(step["entity"])],
            )
            continue
        if op == "compile":
            compiled[str(step["as"])] = db.compile_pattern(
                str(step["namespace"]),
                _source_term(step["subject"], entities),
                str(step["predicate"]),
                _source_term(step["object"], entities),
            )
            continue

        if op == "resolve":
            actual = _normalize_fact(
                db.resolve(SlotId(str(step["slot"]))),
                reverse_entities,
            )
        elif op == "definitions":
            actual = [
                _normalize_fact(fact, reverse_entities)
                for fact in db.definitions(SlotId(str(step["slot"])))
            ]
        elif op == "history_kinds":
            actual = [
                record.kind
                for record in db.history(SlotId(str(step["slot"])))
            ]
        elif op == "display_name_value":
            actual = db.display_name(entities[str(step["entity"])])
        elif op == "query":
            patterns: list[Pattern] = []
            for encoded in step.get("patterns", []):
                if set(encoded) == {"compiled"}:
                    patterns.append(compiled[str(encoded["compiled"])])
                else:
                    patterns.append(_pattern(encoded, entities))
            include_provenance = bool(step.get("include_provenance", False))
            result = db.query(
                patterns,
                optimize=bool(step.get("optimize", True)),
                distinct=bool(step.get("distinct", True)),
                include_provenance=include_provenance,
            )
            actual = {
                "rows": _normalize_rows(result, reverse_entities, include_provenance)
            }
            if "metrics" in step:
                actual["metrics"] = {
                    key: getattr(result.metrics, key)
                    for key in step["metrics"]
                }
        else:
            raise ConformanceFailure(f"Unsupported operation {op!r} in {case['name']}")

        _require_equal(f"{case['name']}::{label}", actual, step["expect"])
        assertions.append({"name": label, "passed": True})

    db.validate()
    return {
        "name": str(case["name"]),
        "passed": True,
        "assertions": assertions,
    }


def _library_projection(report: Mapping[str, Any]) -> dict[str, Any]:
    old_key = (
        "old_compiled_after_atomic_rename_and_rebind"
        if "old_compiled_after_atomic_rename_and_rebind" in report
        else "old_compiled_after_symbol_rebind"
    )
    return _canonical(
        {
            "author": report["author"],
            "copies_and_shelves_initial": report["copies_and_shelves_initial"],
            "alice_holdings": report["alice_holdings"],
            "copy_87_after_move": report["copy_87_after_move"],
            "old_compiled_after_symbol_rebind": report[old_key],
            "new_compiled_after_symbol_rebind": report[
                "new_compiled_after_symbol_rebind"
            ],
            "after_return": report["after_return"],
        }
    )


def _deployment_projection(report: Mapping[str, Any]) -> dict[str, Any]:
    keys = (
        "initial_desired",
        "initial_observed",
        "incompatible_rejected",
        "release_12_desired",
        "drift_after_release",
        "drift_after_worker",
        "drift_after_schema",
        "drift_after_convergence",
        "rollback_desired",
        "rollback_drift",
        "current_release",
        "api_desired_history",
    )
    return _canonical({key: report[key] for key in keys})


def run_conformance() -> dict[str, Any]:
    kernel_fixture = _load_json(CONFORMANCE_V1 / "kernel_cases.json")
    if kernel_fixture.get("schema_version") != 1:
        raise ConformanceFailure("Unsupported kernel conformance schema")

    kernel_cases = [
        _run_kernel_case(case)
        for case in kernel_fixture.get("cases", [])
    ]

    library_expected = _load_json(CONFORMANCE_V1 / "library_expected.json")
    deployment_expected = _load_json(CONFORMANCE_V1 / "deployment_expected.json")

    kernel_library = _library_projection(run_kernel_library())
    kernel_deployment = _deployment_projection(run_kernel_deployment())

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        world_library = _library_projection(
            run_world_library(root / "library.fdb")
        )
        world_deployment = _deployment_projection(
            run_world_deployment(root / "deployment.fdb")
        )

    expected_library = _canonical(library_expected["projection"])
    expected_deployment = _canonical(deployment_expected["projection"])

    _require_equal("library kernel projection", kernel_library, expected_library)
    _require_equal("library committed-world projection", world_library, expected_library)
    _require_equal(
        "deployment kernel projection",
        kernel_deployment,
        expected_deployment,
    )
    _require_equal(
        "deployment committed-world projection",
        world_deployment,
        expected_deployment,
    )

    return {
        "schema_version": 1,
        "overall": "passed",
        "kernel_cases": kernel_cases,
        "applications": {
            "library": {
                "kernel": "passed",
                "committed_world": "passed",
                "projection": expected_library,
            },
            "deployment": {
                "kernel": "passed",
                "committed_world": "passed",
                "projection": expected_deployment,
            },
        },
    }


class ConformanceTests(unittest.TestCase):
    def test_portable_conformance_v1(self) -> None:
        self.assertEqual(run_conformance()["overall"], "passed")


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Verify the language-neutral ForthDB conformance v1 fixtures."
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("artifacts") / "python-conformance-v1.json",
        help="Path for the generated conformance report",
    )
    return parser.parse_args()


def main() -> int:
    args = _parse_args()
    report = run_conformance()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print(json.dumps(report, indent=2, sort_keys=True))
    print(f"Conformance report: {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
