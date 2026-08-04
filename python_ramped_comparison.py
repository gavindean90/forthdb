#!/usr/bin/env python3
"""Compare the durable Python committed-world engine with Rust's ramped trace.

The Python arm deliberately receives the favorable interpretation of rejected
intents: the 64 failed checkout preconditions do not write or synchronize a
Python commit because the original engine has no durable admission journal.
Accepted semantic operations, epoch widths, setup data, and final projections
match the Rust ramped library application.
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
import tempfile
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Callable

from forthdb_kernel import EntityId, Fact, Literal, Pattern, Predicate, SlotId, Variable
from forthdb_world import CommittedWorldDB, WorldTransaction


@dataclass(frozen=True)
class Scale:
    works: int = 10_000
    copies: int = 20_000
    patrons: int = 5_000
    branches: int = 8
    circulation_cycles: int = 64
    intents_per_cycle: int = 8


@dataclass(frozen=True)
class Entities:
    works: list[EntityId]
    copies: list[EntityId]
    patrons: list[EntityId]
    branches: list[EntityId]


Operation = Callable[[WorldTransaction], None]


def relation_slot(owner: EntityId, relation: str, suffix: str = "current") -> SlotId:
    return SlotId(f"{owner.value}/{relation}/{suffix}")


def define(tx: WorldTransaction, slot: SlotId, subject: EntityId, predicate: str, object_: Any) -> None:
    tx.define(slot, Fact(subject, Predicate(predicate), object_))


def display(tx: WorldTransaction, entity: EntityId, value: str) -> None:
    tx.define(
        CommittedWorldDB.display_slot(entity),
        Fact(entity, Predicate("display_name"), Literal(value)),
    )


def allocate_entities(db: CommittedWorldDB, scale: Scale) -> Entities:
    with db.transaction() as tx:
        works = [tx.entity() for _ in range(scale.works)]
        copies = [tx.entity() for _ in range(scale.copies)]
        patrons = [tx.entity() for _ in range(scale.patrons)]
        branches = [tx.entity() for _ in range(scale.branches)]
    return Entities(works, copies, patrons, branches)


def seed_catalog(db: CommittedWorldDB, scale: Scale, entities: Entities) -> None:
    with db.transaction() as tx:
        for index, entity in enumerate(entities.works):
            display(tx, entity, f"Work {index:05}")
            for ordinal in range(2):
                copy = entities.copies[index * 2 + ordinal]
                define(
                    tx,
                    relation_slot(entity, "copy", str(ordinal)),
                    entity,
                    "has_copy",
                    copy,
                )
        for index, entity in enumerate(entities.copies):
            display(tx, entity, f"Copy {index:05}")
            define(
                tx,
                relation_slot(entity, "location"),
                entity,
                "located_at",
                entities.branches[index % scale.branches],
            )
        for index, entity in enumerate(entities.patrons):
            display(tx, entity, f"Patron {index:05}")
        for index, entity in enumerate(entities.branches):
            display(tx, entity, f"Branch {index:02}")


def cycle_operations(scale: Scale, entities: Entities, cycle: int) -> list[Operation]:
    checkout_copy = entities.copies[cycle % len(entities.copies)]
    moved_copy = entities.copies[(cycle + scale.circulation_cycles) % len(entities.copies)]
    recovered_copy = entities.copies[
        (cycle + 2 * scale.circulation_cycles) % len(entities.copies)
    ]
    patron = entities.patrons[cycle % len(entities.patrons)]
    borrower_slot = relation_slot(checkout_copy, "borrower")
    borrower_fact = Fact(checkout_copy, Predicate("borrowed_by"), patron)
    work = entities.works[cycle % len(entities.works)]
    state_slot = relation_slot(recovered_copy, "state")
    lost_fact = Fact(
        recovered_copy,
        Predicate("circulation_state"),
        Literal("lost"),
    )

    return [
        lambda tx: tx.define(borrower_slot, borrower_fact),
        lambda tx: define(
            tx,
            relation_slot(work, "hold", str(cycle)),
            work,
            "held_by",
            patron,
        ),
        lambda tx: define(
            tx,
            relation_slot(moved_copy, "location"),
            moved_copy,
            "located_at",
            entities.branches[(cycle + 1) % len(entities.branches)],
        ),
        lambda tx: tx.define(state_slot, lost_fact),
        lambda tx: define(
            tx,
            state_slot,
            recovered_copy,
            "circulation_state",
            Literal("available"),
        ),
        lambda tx: display(tx, patron, f"Patron {cycle:05} renamed"),
        lambda tx: tx.forget(borrower_slot),
    ]


def query_count(db: CommittedWorldDB, predicate: str, literal: str | None = None) -> int:
    object_: Any = Variable("object") if literal is None else Literal(literal)
    return len(
        db.query(
            [Pattern(Variable("subject"), Predicate(predicate), object_)]
        ).rows
    )


def projection(db: CommittedWorldDB) -> dict[str, int]:
    return {
        "active_slots": db.active_slots,
        "immutable_records": db.immutable_records,
        "checked_out_copies": query_count(db, "borrowed_by"),
        "active_holds": query_count(db, "held_by"),
        "available_after_recovery": query_count(
            db, "circulation_state", "available"
        ),
        "located_copies": query_count(db, "located_at"),
    }


def summarize_us(samples: list[int]) -> dict[str, float | int]:
    ordered = sorted(samples)
    return {
        "median_us": statistics.median(ordered),
        "p95_us": ordered[(len(ordered) - 1) * 95 // 100],
        "maximum_us": ordered[-1],
    }


def run_profile(path: Path, scale: Scale, epoch_width: int) -> dict[str, Any]:
    db = CommittedWorldDB.open(path)
    setup_started = time.perf_counter_ns()
    entities = allocate_entities(db, scale)
    seed_catalog(db, scale, entities)
    setup_us = (time.perf_counter_ns() - setup_started) // 1_000
    print(
        f"python width {epoch_width}: setup complete in {setup_us / 1_000_000:.2f}s",
        file=sys.stderr,
        flush=True,
    )

    operations = [cycle_operations(scale, entities, cycle) for cycle in range(scale.circulation_cycles)]
    commit_latencies: list[int] = []
    workload_started = time.perf_counter_ns()
    if epoch_width == 1:
        for cycle, accepted in enumerate(operations):
            # The second intent in every cycle is the contested checkout. It is
            # rejected by reading the already-defined borrower slot and receives
            # no Python fsync, favoring the Python control.
            borrower = relation_slot(entities.copies[cycle], "borrower")
            for position, operation in enumerate(accepted):
                started = time.perf_counter_ns()
                with db.transaction() as tx:
                    operation(tx)
                commit_latencies.append((time.perf_counter_ns() - started) // 1_000)
                if position == 0 and db.resolve(borrower) is None:
                    raise AssertionError("checkout did not become visible")
            if db.resolve(borrower) is not None:
                raise AssertionError("return did not clear the borrower")
            if (cycle + 1) % 8 == 0:
                elapsed = (time.perf_counter_ns() - workload_started) / 1_000_000_000
                print(
                    f"python width 1: {cycle + 1}/{scale.circulation_cycles} cycles in {elapsed:.1f}s",
                    file=sys.stderr,
                    flush=True,
                )
        durable_commits = scale.circulation_cycles * 7
    else:
        cycles_per_epoch = epoch_width // scale.intents_per_cycle
        for start in range(0, scale.circulation_cycles, cycles_per_epoch):
            started = time.perf_counter_ns()
            with db.transaction() as tx:
                for accepted in operations[start : start + cycles_per_epoch]:
                    for operation in accepted:
                        operation(tx)
            commit_latencies.append((time.perf_counter_ns() - started) // 1_000)
            completed = (start // cycles_per_epoch) + 1
            if completed % 4 == 0:
                elapsed = (time.perf_counter_ns() - workload_started) / 1_000_000_000
                print(
                    f"python width {epoch_width}: {completed}/{scale.circulation_cycles // cycles_per_epoch} epochs in {elapsed:.1f}s",
                    file=sys.stderr,
                    flush=True,
                )
        durable_commits = scale.circulation_cycles // cycles_per_epoch
    workload_us = (time.perf_counter_ns() - workload_started) // 1_000

    query_started = time.perf_counter_ns()
    live_projection = projection(db)
    query_us = (time.perf_counter_ns() - query_started) // 1_000
    expected_digest = db.digest
    expected_version = db.version
    recovery_started = time.perf_counter_ns()
    recovered = CommittedWorldDB.open(path)
    recovery_open_us = (time.perf_counter_ns() - recovery_started) // 1_000
    recovered_projection = projection(recovered)
    recovery_query_ready_us = (time.perf_counter_ns() - recovery_started) // 1_000
    if recovered.digest != expected_digest or recovered.version != expected_version:
        raise AssertionError("Python recovery changed the committed world")
    if recovered_projection != live_projection:
        raise AssertionError("Python recovery changed the semantic projection")
    print(
        f"python width {epoch_width}: recovery and projection parity complete",
        file=sys.stderr,
        flush=True,
    )

    intent_count = scale.circulation_cycles * scale.intents_per_cycle
    return {
        "epoch_width": epoch_width,
        "setup_elapsed_us": setup_us,
        "workload_elapsed_us": workload_us,
        "intents_per_second": intent_count / (workload_us / 1_000_000),
        "accepted_intents": scale.circulation_cycles * 7,
        "rejected_intents": scale.circulation_cycles,
        "durable_commits": durable_commits,
        "syncs_per_intent": durable_commits / intent_count,
        "commit_latency": summarize_us(commit_latencies),
        "query_elapsed_us": query_us,
        "recovery_open_elapsed_us": recovery_open_us,
        "recovery_query_ready_us": recovery_query_ready_us,
        "journal_bytes": path.stat().st_size,
        "projection": live_projection,
    }


def compare(rust: dict[str, Any], python: dict[str, Any]) -> dict[str, Any]:
    output: dict[str, Any] = {}
    for profile in ("interactive", "branch_rush"):
        rust_profile = rust[profile]
        python_profile = python[profile]
        if rust_profile["projection"] != python_profile["projection"]:
            raise RuntimeError(f"{profile} Python and Rust projections differ")
        output[profile] = {
            "python": python_profile,
            "rust": {
                "epoch_width": rust_profile["epoch_width"],
                "workload_elapsed_us": rust_profile["workload_elapsed_us"],
                "intents_per_second": rust_profile["intents_per_second"],
                "durable_epochs": rust_profile["durable_epochs"],
                "syncs_per_intent": rust_profile["syncs_per_intent"],
                "projection": rust_profile["projection"],
            },
            "rust_to_python_throughput_ratio": (
                rust_profile["intents_per_second"]
                / python_profile["intents_per_second"]
            ),
        }
    return output


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rust-report", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    rust = json.loads(arguments.rust_report.read_text(encoding="utf-8"))
    scale = Scale(**rust["scale"])
    with tempfile.TemporaryDirectory(prefix="forthdb-python-ramped-") as directory:
        root = Path(directory)
        branch_rush = run_profile(root / "branch-rush.fdb", scale, 16)
        interactive = run_profile(root / "interactive.fdb", scale, 1)
        python = {"interactive": interactive, "branch_rush": branch_rush}
    report = {
        "status": "observational",
        "scope": "complete-ramped-library-python-versus-rust",
        "scale": asdict(scale),
        "comparison": compare(rust, python),
        "caveat": (
            "Python favors rejected intents by giving them no durable admission sync; "
            "Rust durably admits all 512 intents before semantic acceptance or rejection."
        ),
    }
    arguments.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
