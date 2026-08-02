from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import platform
import subprocess
import sys
import tempfile
import unittest
from dataclasses import dataclass, asdict
from pathlib import Path
from typing import Any, Iterable

from atomic_demo import AtomicKernelTests
from library_demo import KernelRegressionTests, run_library as run_kernel_library
from world_library_demo import CommittedWorldTests, run_library as run_world_library


REPORT_SCHEMA_VERSION = 1


@dataclass(frozen=True)
class SuiteEvidence:
    name: str
    passed: bool
    tests_run: int
    failures: int
    errors: int
    skipped: int
    output: str


@dataclass(frozen=True)
class CheckEvidence:
    name: str
    passed: bool
    detail: str


def _canonical_rows(rows: Iterable[dict[str, str]]) -> list[dict[str, str]]:
    return sorted(
        (dict(row) for row in rows),
        key=lambda row: json.dumps(row, sort_keys=True, separators=(",", ":")),
    )


def _kernel_projection(report: dict[str, Any]) -> dict[str, Any]:
    return {
        "author": _canonical_rows(report["author"]),
        "copies_and_shelves_initial": _canonical_rows(report["copies_and_shelves_initial"]),
        "alice_holdings": _canonical_rows(report["alice_holdings"]),
        "copy_87_after_move": _canonical_rows(report["copy_87_after_move"]),
        "old_compiled_after_symbol_rebind": _canonical_rows(
            report["old_compiled_after_symbol_rebind"]
        ),
        "new_compiled_after_symbol_rebind": _canonical_rows(
            report["new_compiled_after_symbol_rebind"]
        ),
        "after_return": _canonical_rows(report["after_return"]),
    }


def _world_projection(report: dict[str, Any]) -> dict[str, Any]:
    return {
        "author": _canonical_rows(report["author"]),
        "copies_and_shelves_initial": _canonical_rows(report["copies_and_shelves_initial"]),
        "alice_holdings": _canonical_rows(report["alice_holdings"]),
        "copy_87_after_move": _canonical_rows(report["copy_87_after_move"]),
        "old_compiled_after_symbol_rebind": _canonical_rows(
            report["old_compiled_after_atomic_rename_and_rebind"]
        ),
        "new_compiled_after_symbol_rebind": _canonical_rows(
            report["new_compiled_after_symbol_rebind"]
        ),
        "after_return": _canonical_rows(report["after_return"]),
    }


def _run_suite(name: str, cases: Iterable[type[unittest.TestCase]]) -> SuiteEvidence:
    suite = unittest.TestSuite()
    loader = unittest.defaultTestLoader
    for case in cases:
        suite.addTests(loader.loadTestsFromTestCase(case))

    stream = io.StringIO()
    result = unittest.TextTestRunner(stream=stream, verbosity=2).run(suite)
    return SuiteEvidence(
        name=name,
        passed=result.wasSuccessful(),
        tests_run=result.testsRun,
        failures=len(result.failures),
        errors=len(result.errors),
        skipped=len(result.skipped),
        output=stream.getvalue(),
    )


def _check(checks: list[CheckEvidence], name: str, condition: bool, detail: str) -> None:
    checks.append(CheckEvidence(name=name, passed=bool(condition), detail=detail))


def _sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _run_world_worker(seed: int, directory: Path) -> tuple[dict[str, Any], bytes]:
    database_path = directory / "library.fdb"
    report_path = directory / "world-report.json"
    command = [
        sys.executable,
        str(Path(__file__).resolve()),
        "--world-worker",
        "--database-path",
        str(database_path),
        "--worker-report",
        str(report_path),
    ]
    environment = dict(os.environ)
    environment["PYTHONHASHSEED"] = str(seed)
    completed = subprocess.run(
        command,
        check=False,
        capture_output=True,
        text=True,
        env=environment,
    )
    if completed.returncode != 0:
        raise RuntimeError(
            f"world worker with PYTHONHASHSEED={seed} failed\n"
            f"stdout:\n{completed.stdout}\n"
            f"stderr:\n{completed.stderr}"
        )
    return json.loads(report_path.read_text(encoding="utf-8")), database_path.read_bytes()


def _render_markdown(report: dict[str, Any]) -> str:
    lines = [
        "# ForthDB Research Regression",
        "",
        f"**Overall:** `{report['overall']}`",
        "",
        "## Component suites",
        "",
        "| Suite | Status | Tests | Failures | Errors | Skipped |",
        "| --- | --- | ---: | ---: | ---: | ---: |",
    ]

    for suite in report["suites"]:
        status = "passed" if suite["passed"] else "failed"
        lines.append(
            f"| {suite['name']} | {status} | {suite['tests_run']} | "
            f"{suite['failures']} | {suite['errors']} | {suite['skipped']} |"
        )

    lines.extend(
        [
            "",
            "## Research checks",
            "",
            "| Check | Status | Detail |",
            "| --- | --- | --- |",
        ]
    )
    for check in report["checks"]:
        status = "passed" if check["passed"] else "failed"
        escaped_detail = check["detail"].replace("|", "\\|").replace("\n", " ")
        lines.append(f"| {check['name']} | {status} | {escaped_detail} |")

    observations = report.get("observations", {})
    lines.extend(["", "## Observations", ""])
    lines.append(f"- Python: `{report['environment']['python']}`")
    lines.append(f"- Platform: `{report['environment']['platform']}`")
    if observations:
        lines.append(f"- Kernel active slots: `{observations.get('kernel_active_slots')}`")
        lines.append(f"- Kernel immutable records: `{observations.get('kernel_immutable_records')}`")
        lines.append(f"- Committed-world active slots: `{observations.get('world_active_slots')}`")
        lines.append(f"- Committed-world immutable records: `{observations.get('world_immutable_records')}`")
        lines.append(f"- Final world version: `{observations.get('world_version')}`")
        lines.append(f"- Final world digest: `{observations.get('world_digest')}`")
        lines.append(f"- Commit-log bytes: `{observations.get('log_bytes')}`")
        lines.append(f"- Commit-log SHA-256: `{observations.get('log_sha256')}`")

    lines.extend(["", "## Shared semantic projection", "", "```json"])
    lines.append(json.dumps(report.get("semantic_projection", {}), indent=2, sort_keys=True))
    lines.append("```")

    failed_suites = [suite for suite in report["suites"] if not suite["passed"]]
    if failed_suites:
        lines.extend(["", "## Failed suite output", ""])
        for suite in failed_suites:
            lines.extend(
                [
                    f"### {suite['name']}",
                    "",
                    "```text",
                    suite["output"].rstrip(),
                    "```",
                    "",
                ]
            )

    return "\n".join(lines).rstrip() + "\n"


def run_research_regression(output_dir: Path) -> dict[str, Any]:
    output_dir.mkdir(parents=True, exist_ok=True)

    suites = [
        _run_suite("semantic kernel", [KernelRegressionTests]),
        _run_suite("historical atomic model", [AtomicKernelTests]),
        _run_suite("committed-world ACID model", [CommittedWorldTests]),
    ]
    checks: list[CheckEvidence] = []
    observations: dict[str, Any] = {}
    semantic_projection: dict[str, Any] = {}

    kernel_report: dict[str, Any] | None = None
    world_report_a: dict[str, Any] | None = None
    world_report_b: dict[str, Any] | None = None
    log_a = b""
    log_b = b""

    try:
        kernel_report = run_kernel_library()
    except Exception as exc:  # pragma: no cover - exercised only on regression failure
        _check(checks, "kernel library executes", False, repr(exc))
    else:
        _check(checks, "kernel library executes", True, "baseline application completed")
        observations["kernel_active_slots"] = kernel_report["active_slots"]
        observations["kernel_immutable_records"] = kernel_report["immutable_records"]

    try:
        with tempfile.TemporaryDirectory() as first_dir, tempfile.TemporaryDirectory() as second_dir:
            world_report_a, log_a = _run_world_worker(1, Path(first_dir))
            world_report_b, log_b = _run_world_worker(2, Path(second_dir))
    except Exception as exc:  # pragma: no cover - exercised only on regression failure
        _check(checks, "committed-world library executes twice", False, repr(exc))
    else:
        _check(
            checks,
            "committed-world library executes twice",
            True,
            "separate Python processes with different hash seeds completed independently",
        )
        observations.update(
            {
                "world_active_slots": world_report_a["active_slots"],
                "world_immutable_records": world_report_a["immutable_records"],
                "world_version": world_report_a["world_version"],
                "world_digest": world_report_a["world_digest"],
                "log_bytes": len(log_a),
                "log_sha256": _sha256_bytes(log_a),
            }
        )

    if kernel_report is not None and world_report_a is not None:
        kernel_projection = _kernel_projection(kernel_report)
        world_projection = _world_projection(world_report_a)
        semantic_projection = {
            "kernel": kernel_projection,
            "committed_world": world_projection,
        }
        _check(
            checks,
            "cross-model semantic continuity",
            kernel_projection == world_projection,
            "shared library projection is identical",
        )

        expected_recovery_rows = _canonical_rows(
            [
                {"copy": "Copy 42", "shelf": "Shelf A3"},
                {"copy": "Copy 87", "shelf": "Shelf C3"},
            ]
        )
        actual_recovery_rows = _canonical_rows(world_report_a["recovery"]["copies_and_shelves"])
        _check(
            checks,
            "recovery matches live committed world",
            bool(world_report_a["recovery"]["same_version"])
            and bool(world_report_a["recovery"]["same_digest"])
            and actual_recovery_rows == expected_recovery_rows,
            "version, digest, and final library locations survive restart",
        )

    if world_report_a is not None and world_report_b is not None:
        _check(
            checks,
            "deterministic world identity",
            world_report_a["world_digest"] == world_report_b["world_digest"],
            "independent runs produce the same world digest",
        )
        _check(
            checks,
            "deterministic durable bytes",
            log_a == log_b,
            "independent runs produce byte-identical commit logs",
        )
        _check(
            checks,
            "deterministic application report",
            _world_projection(world_report_a) == _world_projection(world_report_b),
            "independent runs produce the same shared semantic projection",
        )

    all_passed = all(suite.passed for suite in suites) and all(check.passed for check in checks)
    report: dict[str, Any] = {
        "schema_version": REPORT_SCHEMA_VERSION,
        "overall": "passed" if all_passed else "failed",
        "environment": {
            "python": platform.python_version(),
            "platform": platform.platform(),
            "implementation": platform.python_implementation(),
        },
        "suites": [asdict(suite) for suite in suites],
        "checks": [asdict(check) for check in checks],
        "observations": observations,
        "semantic_projection": semantic_projection,
    }

    json_path = output_dir / "regression-report.json"
    markdown_path = output_dir / "regression-report.md"
    json_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    markdown_path.write_text(_render_markdown(report), encoding="utf-8")

    print(markdown_path.read_text(encoding="utf-8"))
    print(f"JSON report: {json_path}")
    print(f"Markdown report: {markdown_path}")

    return report


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run the complete ForthDB research regression and emit evidence reports."
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=Path("artifacts"),
        help="Directory for regression-report.json and regression-report.md",
    )
    parser.add_argument("--world-worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--database-path", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--worker-report", type=Path, help=argparse.SUPPRESS)
    return parser.parse_args()


def main() -> int:
    args = _parse_args()
    if args.world_worker:
        if args.database_path is None or args.worker_report is None:
            raise SystemExit("worker mode requires --database-path and --worker-report")
        worker_report = run_world_library(args.database_path)
        args.worker_report.parent.mkdir(parents=True, exist_ok=True)
        args.worker_report.write_text(
            json.dumps(worker_report, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        return 0

    report = run_research_regression(args.output_dir)
    return 0 if report["overall"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
