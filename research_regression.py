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
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Callable, Iterable

from atomic_demo import AtomicKernelTests
from deployment_demo import DeploymentKernelTests, run_deployment as run_kernel_deployment
from library_demo import KernelRegressionTests, run_library as run_kernel_library
from world_deployment_demo import WorldDeploymentTests, run_deployment as run_world_deployment
from world_library_demo import CommittedWorldTests, run_library as run_world_library


REPORT_SCHEMA_VERSION = 2


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


def _library_kernel_projection(report: dict[str, Any]) -> dict[str, Any]:
    return _canonical(
        {
            "author": report["author"],
            "copies_and_shelves_initial": report["copies_and_shelves_initial"],
            "alice_holdings": report["alice_holdings"],
            "copy_87_after_move": report["copy_87_after_move"],
            "old_compiled_after_symbol_rebind": report["old_compiled_after_symbol_rebind"],
            "new_compiled_after_symbol_rebind": report["new_compiled_after_symbol_rebind"],
            "after_return": report["after_return"],
        }
    )


def _library_world_projection(report: dict[str, Any]) -> dict[str, Any]:
    return _canonical(
        {
            "author": report["author"],
            "copies_and_shelves_initial": report["copies_and_shelves_initial"],
            "alice_holdings": report["alice_holdings"],
            "copy_87_after_move": report["copy_87_after_move"],
            "old_compiled_after_symbol_rebind": report[
                "old_compiled_after_atomic_rename_and_rebind"
            ],
            "new_compiled_after_symbol_rebind": report["new_compiled_after_symbol_rebind"],
            "after_return": report["after_return"],
        }
    )


def _deployment_projection(report: dict[str, Any]) -> dict[str, Any]:
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


def _record(checks: list[CheckEvidence], name: str, passed: bool, detail: str) -> None:
    checks.append(CheckEvidence(name, bool(passed), detail))


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _run_worker(kind: str, seed: int, directory: Path) -> tuple[dict[str, Any], bytes]:
    database_path = directory / f"{kind}.fdb"
    report_path = directory / f"{kind}-report.json"
    command = [
        sys.executable,
        str(Path(__file__).resolve()),
        "--worker",
        kind,
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
            f"{kind} worker with PYTHONHASHSEED={seed} failed\n"
            f"stdout:\n{completed.stdout}\n"
            f"stderr:\n{completed.stderr}"
        )
    return json.loads(report_path.read_text(encoding="utf-8")), database_path.read_bytes()


def _exercise_world_model(
    *,
    kind: str,
    checks: list[CheckEvidence],
    observations: dict[str, Any],
    projection: Callable[[dict[str, Any]], dict[str, Any]],
) -> tuple[dict[str, Any] | None, dict[str, Any] | None, bytes, bytes]:
    first_report: dict[str, Any] | None = None
    second_report: dict[str, Any] | None = None
    first_log = b""
    second_log = b""
    try:
        with tempfile.TemporaryDirectory() as first_dir, tempfile.TemporaryDirectory() as second_dir:
            first_report, first_log = _run_worker(kind, 1, Path(first_dir))
            second_report, second_log = _run_worker(kind, 2, Path(second_dir))
    except Exception as exc:  # pragma: no cover - only on regression failure
        _record(checks, f"{kind} committed-world application executes twice", False, repr(exc))
        return None, None, b"", b""

    _record(
        checks,
        f"{kind} committed-world application executes twice",
        True,
        "separate Python processes with different hash seeds completed independently",
    )
    observations[kind] = {
        "active_slots": first_report["active_slots"],
        "immutable_records": first_report["immutable_records"],
        "world_version": first_report["world_version"],
        "world_digest": first_report["world_digest"],
        "log_bytes": len(first_log),
        "log_sha256": _sha256(first_log),
    }
    _record(
        checks,
        f"{kind} deterministic world identity",
        first_report["world_digest"] == second_report["world_digest"],
        "independent runs produce the same world digest",
    )
    _record(
        checks,
        f"{kind} deterministic durable bytes",
        first_log == second_log,
        "independent runs produce byte-identical commit logs",
    )
    _record(
        checks,
        f"{kind} deterministic application projection",
        projection(first_report) == projection(second_report),
        "independent runs produce the same semantic projection",
    )
    return first_report, second_report, first_log, second_log


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
        lines.append(
            f"| {suite['name']} | {'passed' if suite['passed'] else 'failed'} | "
            f"{suite['tests_run']} | {suite['failures']} | {suite['errors']} | {suite['skipped']} |"
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
        detail = check["detail"].replace("|", "\\|").replace("\n", " ")
        lines.append(
            f"| {check['name']} | {'passed' if check['passed'] else 'failed'} | {detail} |"
        )

    lines.extend(["", "## Observations", ""])
    lines.append(f"- Python: `{report['environment']['python']}`")
    lines.append(f"- Platform: `{report['environment']['platform']}`")
    for name, observation in report.get("observations", {}).items():
        lines.append(f"- **{name}**")
        for key, value in observation.items():
            lines.append(f"  - {key.replace('_', ' ')}: `{value}`")

    lines.extend(["", "## Cross-model semantic projections", "", "```json"])
    lines.append(json.dumps(report.get("semantic_projections", {}), indent=2, sort_keys=True))
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
        _run_suite("deployment semantic application", [DeploymentKernelTests]),
        _run_suite("deployment committed-world application", [WorldDeploymentTests]),
    ]
    checks: list[CheckEvidence] = []
    observations: dict[str, Any] = {}
    projections: dict[str, Any] = {}

    kernel_library: dict[str, Any] | None = None
    try:
        kernel_library = run_kernel_library()
    except Exception as exc:  # pragma: no cover
        _record(checks, "library kernel application executes", False, repr(exc))
    else:
        _record(checks, "library kernel application executes", True, "baseline application completed")
        observations["library_kernel"] = {
            "active_slots": kernel_library["active_slots"],
            "immutable_records": kernel_library["immutable_records"],
        }

    world_library, _, _, _ = _exercise_world_model(
        kind="library",
        checks=checks,
        observations=observations,
        projection=_library_world_projection,
    )
    if kernel_library is not None and world_library is not None:
        kernel_projection = _library_kernel_projection(kernel_library)
        world_projection = _library_world_projection(world_library)
        projections["library"] = {"kernel": kernel_projection, "committed_world": world_projection}
        _record(
            checks,
            "library cross-model semantic continuity",
            kernel_projection == world_projection,
            "shared library projection is identical",
        )
        recovery = world_library["recovery"]
        _record(
            checks,
            "library recovery matches live committed world",
            bool(recovery["same_version"]) and bool(recovery["same_digest"]),
            "version and digest survive restart",
        )

    kernel_deployment: dict[str, Any] | None = None
    try:
        kernel_deployment = run_kernel_deployment()
    except Exception as exc:  # pragma: no cover
        _record(checks, "deployment kernel application executes", False, repr(exc))
    else:
        _record(
            checks,
            "deployment kernel application executes",
            True,
            "deployment control-plane application completed",
        )
        observations["deployment_kernel"] = {
            "active_slots": kernel_deployment["active_slots"],
            "immutable_records": kernel_deployment["immutable_records"],
        }

    world_deployment, _, _, _ = _exercise_world_model(
        kind="deployment",
        checks=checks,
        observations=observations,
        projection=_deployment_projection,
    )
    if kernel_deployment is not None and world_deployment is not None:
        kernel_projection = _deployment_projection(kernel_deployment)
        world_projection = _deployment_projection(world_deployment)
        projections["deployment"] = {
            "kernel": kernel_projection,
            "committed_world": world_projection,
        }
        _record(
            checks,
            "deployment cross-model semantic continuity",
            kernel_projection == world_projection,
            "desired state, drift, convergence, rollback, and history are identical",
        )
        recovery = world_deployment["recovery"]
        _record(
            checks,
            "deployment recovery matches live committed world",
            bool(recovery["same_version"])
            and bool(recovery["same_digest"])
            and recovery["desired"] == world_deployment["rollback_desired"]
            and recovery["drift"] == world_deployment["rollback_drift"],
            "rollback world, drift, version, and digest survive restart",
        )

    passed = all(suite.passed for suite in suites) and all(check.passed for check in checks)
    report: dict[str, Any] = {
        "schema_version": REPORT_SCHEMA_VERSION,
        "overall": "passed" if passed else "failed",
        "environment": {
            "python": platform.python_version(),
            "platform": platform.platform(),
            "implementation": platform.python_implementation(),
        },
        "suites": [asdict(suite) for suite in suites],
        "checks": [asdict(check) for check in checks],
        "observations": observations,
        "semantic_projections": projections,
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
    parser.add_argument("--worker", choices=("library", "deployment"), help=argparse.SUPPRESS)
    parser.add_argument("--database-path", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--worker-report", type=Path, help=argparse.SUPPRESS)
    return parser.parse_args()


def main() -> int:
    args = _parse_args()
    if args.worker:
        if args.database_path is None or args.worker_report is None:
            raise SystemExit("worker mode requires --database-path and --worker-report")
        runner = run_world_library if args.worker == "library" else run_world_deployment
        worker_report = runner(args.database_path)
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
