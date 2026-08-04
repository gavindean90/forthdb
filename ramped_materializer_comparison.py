#!/usr/bin/env python3
"""Alternating-order comparison of VM roots and eager world materialization."""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import tempfile
from pathlib import Path


ENGINES = ("vm", "world")
PROFILES = ("interactive", "branch_rush")


def run_engine(binary: Path, root: Path, engine: str) -> dict:
    environment = os.environ.copy()
    environment["FORTHDB_LIBRARY_MATERIALIZER"] = engine
    completed = subprocess.run(
        [str(binary), str(root)],
        check=True,
        capture_output=True,
        text=True,
        env=environment,
    )
    return json.loads(completed.stdout)


def median(samples: list[float | int]) -> float:
    return float(statistics.median(samples))


def summarize(reports: list[dict], profile: str) -> dict:
    selected = [report[profile] for report in reports]
    return {
        "samples": len(selected),
        "intents_per_second": median(
            [sample["intents_per_second"] for sample in selected]
        ),
        "workload_elapsed_us": median(
            [sample["workload_elapsed_us"] for sample in selected]
        ),
        "setup_elapsed_us": median(
            [sample["setup_elapsed_us"] for sample in selected]
        ),
        "query_projection_elapsed_us": median(
            [sample["query_projection_elapsed_us"] for sample in selected]
        ),
        "recovery_open_elapsed_us": median(
            [sample["recovery"]["open_elapsed_us"] for sample in selected]
        ),
        "recovery_query_projection_elapsed_us": median(
            [
                sample["recovery"]["query_projection_elapsed_us"]
                for sample in selected
            ]
        ),
        "full_replay_query_ready_us": median(
            [sample["recovery"]["full_replay_query_ready_us"] for sample in selected]
        ) if selected[0]["recovery"]["full_replay_query_ready_us"] is not None else None,
        "mmap_query_ready_us": median(
            [sample["recovery"]["mmap_query_ready_us"] for sample in selected]
        ) if selected[0]["recovery"]["mmap_query_ready_us"] is not None else None,
        "mmap_snapshot_creation_us": median(
            [sample["recovery"]["mmap_snapshot_creation_us"] for sample in selected]
        ) if selected[0]["recovery"]["mmap_snapshot_creation_us"] is not None else None,
        "mmap_snapshot_bytes": median(
            [sample["recovery"]["mmap_snapshot_bytes"] for sample in selected]
        ) if selected[0]["recovery"]["mmap_snapshot_bytes"] is not None else None,
        "throughput_samples": [
            sample["intents_per_second"] for sample in selected
        ],
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=4)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    if arguments.rounds < 2:
        parser.error("--rounds must be at least two")

    reports = {engine: [] for engine in ENGINES}
    orders = []
    with tempfile.TemporaryDirectory(prefix="forthdb-ramped-comparison-") as temporary:
        temporary_root = Path(temporary)
        for round_index in range(arguments.rounds):
            order = ENGINES if round_index % 2 == 0 else tuple(reversed(ENGINES))
            orders.append(list(order))
            for engine in order:
                report = run_engine(
                    arguments.binary,
                    temporary_root / f"round-{round_index}-{engine}",
                    engine,
                )
                if not report["semantic_projection_equal"]:
                    raise RuntimeError(f"{engine} round {round_index} lost semantic parity")
                reports[engine].append(report)

    summarized = {
        engine: {
            profile: summarize(reports[engine], profile) for profile in PROFILES
        }
        for engine in ENGINES
    }
    result = {
        "status": "observational",
        "scope": "alternating-order-ramped-materializer-comparison",
        "rounds": arguments.rounds,
        "orders": orders,
        "vm": summarized["vm"],
        "world": summarized["world"],
    }
    for profile in PROFILES:
        result[f"{profile}_vm_to_world_ratio"] = (
            summarized["vm"][profile]["intents_per_second"]
            / summarized["world"][profile]["intents_per_second"]
        )
        full = summarized["vm"][profile]["full_replay_query_ready_us"]
        mapped = summarized["vm"][profile]["mmap_query_ready_us"]
        result[f"{profile}_mmap_recovery_speedup"] = full / mapped
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
