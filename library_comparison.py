from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any

from world_library_demo import run_library as run_python_library


def timed_python(path: Path) -> tuple[int, dict[str, Any]]:
    started = time.perf_counter_ns()
    report = run_python_library(path)
    return (time.perf_counter_ns() - started) // 1_000, report


def timed_rust(binary: Path, path: Path, report_path: Path) -> tuple[int, dict[str, Any]]:
    environment = dict(os.environ)
    environment["FORTHDB_LIBRARY_REPORT"] = str(report_path)
    completed = subprocess.run(
        [str(binary), str(path)],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        env=environment,
    )
    if completed.returncode != 0:
        raise RuntimeError(f"Rust library run failed: {completed.stderr}")
    report = json.loads(report_path.read_text(encoding="utf-8"))
    return int(report["elapsed_us"]), report


def projection(report: dict[str, Any], rust: bool) -> dict[str, Any]:
    return {
        "author": report["author"],
        "copies": report["copies_and_shelves_initial"],
        "holdings": report["alice_holdings"],
        "moved": report["copy_87_after_move"],
        "old_compiled": report[
            "old_compiled_after_rename_and_rebind"
            if rust
            else "old_compiled_after_atomic_rename_and_rebind"
        ],
        "new_compiled": report["new_compiled_after_symbol_rebind"],
        "returned": report["after_return"],
    }


def summarize(samples: list[int]) -> dict[str, Any]:
    return {
        "median_us": statistics.median(samples),
        "minimum_us": min(samples),
        "maximum_us": max(samples),
        "samples_us": samples,
    }


def run(binary: Path, rounds: int) -> dict[str, Any]:
    python_samples: list[int] = []
    rust_samples: list[int] = []
    python_report: dict[str, Any] | None = None
    rust_report: dict[str, Any] | None = None

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        # Warm both implementations without recording their coldest run.
        timed_python(root / "python-warmup.fdb")
        timed_rust(
            binary,
            root / "rust-warmup.fdb",
            root / "rust-warmup.json",
        )

        for round_index in range(rounds):
            # Alternate order to spread filesystem and scheduling position.
            if round_index % 2 == 0:
                python_us, python_report = timed_python(
                    root / f"python-{round_index}.fdb"
                )
                rust_us, rust_report = timed_rust(
                    binary,
                    root / f"rust-{round_index}.fdb",
                    root / f"rust-{round_index}.json",
                )
            else:
                rust_us, rust_report = timed_rust(
                    binary,
                    root / f"rust-{round_index}.fdb",
                    root / f"rust-{round_index}.json",
                )
                python_us, python_report = timed_python(
                    root / f"python-{round_index}.fdb"
                )
            python_samples.append(python_us)
            rust_samples.append(rust_us)

    assert python_report is not None and rust_report is not None
    python_projection = projection(python_report, rust=False)
    rust_projection = projection(rust_report, rust=True)
    if python_projection != rust_projection:
        raise RuntimeError("Python and Rust library projections differ")

    python_summary = summarize(python_samples)
    rust_summary = summarize(rust_samples)
    return {
        "status": "observational",
        "scope": "complete-library-scenario-excluding-process-startup",
        "rounds": rounds,
        "python": {
            "engine": "python_committed_world_write_fsync",
            "durable_commits_per_round": 6,
            **python_summary,
        },
        "rust": {
            "engine": "rust_speculative_io_uring_one_epoch_ahead",
            "durable_commits_per_round": rust_report["controller"]["epochs"],
            "speculative_epochs_prepared_last_round": rust_report["controller"][
                "speculative_epochs_prepared"
            ],
            "speculative_epochs_rederived_last_round": rust_report["controller"][
                "speculative_epochs_rederived"
            ],
            **rust_summary,
        },
        "python_to_rust_median_ratio": (
            python_summary["median_us"] / rust_summary["median_us"]
        ),
        "semantic_projection_equal": True,
        "caveat": (
            "The implementations preserve the same application projection, but Rust uses "
            "seven durable epochs because entity allocation and metadata attachment are "
            "separate; Python uses six. Timings are hosted-runner observations."
        ),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rust-binary", required=True, type=Path)
    parser.add_argument("--rounds", type=int, default=15)
    parser.add_argument("--output", required=True, type=Path)
    arguments = parser.parse_args()
    report = run(arguments.rust_binary.resolve(), arguments.rounds)
    arguments.output.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print(json.dumps(report, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
