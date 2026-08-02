from __future__ import annotations

import json
import os
import tempfile
import unittest
from pathlib import Path

from forthdb_kernel import EntityId, Fact, Literal, Pattern, Predicate, SlotId, Symbol, Variable
from forthdb_world import (
    CommittedWorldDB,
    LogCorruption,
    TransactionConflict,
)


class WorldLibraryHarness:
    NS = "library"

    def __init__(self, path: Path) -> None:
        self.db = CommittedWorldDB.open(path)

    @staticmethod
    def relation_slot(owner: EntityId, relation: str, suffix: str = "current") -> SlotId:
        return SlotId(f"{owner.value}/{relation}/{suffix}")

    def seed_entities(self) -> dict[str, EntityId]:
        named_values = [
            ("Asimov", "Isaac Asimov"),
            ("Foundation", "Foundation"),
            ("Science_Fiction", "Science Fiction"),
            ("Copy_42", "Copy 42"),
            ("Copy_87", "Copy 87"),
            ("Shelf_A3", "Shelf A3"),
            ("Shelf_B1", "Shelf B1"),
            ("Shelf_C3", "Shelf C3"),
            ("Alice", "Alice"),
            ("Bob", "Bob"),
        ]
        entities: dict[str, EntityId] = {}
        with self.db.transaction() as tx:
            for symbol, display in named_values:
                entity = tx.entity()
                entities[symbol] = entity
                self.db.define_display_name(tx, entity, display)
                self.db.bind_symbol(tx, self.NS, Symbol(symbol), entity)
        return entities


def run_library(path: Path) -> dict:
    lib = WorldLibraryHarness(path)
    db = lib.db
    entities = lib.seed_entities()

    asimov = entities["Asimov"]
    foundation = entities["Foundation"]
    science_fiction = entities["Science_Fiction"]
    copy_42 = entities["Copy_42"]
    copy_87 = entities["Copy_87"]
    shelf_a3 = entities["Shelf_A3"]
    shelf_b1 = entities["Shelf_B1"]
    shelf_c3 = entities["Shelf_C3"]
    alice = entities["Alice"]
    bob = entities["Bob"]

    # The initial catalog enters as one committed world transition.
    with db.transaction() as tx:
        tx.define(lib.relation_slot(foundation, "author"), Fact(foundation, Predicate("written_by"), asimov))
        tx.define(lib.relation_slot(foundation, "subject", "sf"), Fact(foundation, Predicate("subject"), science_fiction))
        tx.define(lib.relation_slot(foundation, "copy", "42"), Fact(foundation, Predicate("has_copy"), copy_42))
        tx.define(lib.relation_slot(foundation, "copy", "87"), Fact(foundation, Predicate("has_copy"), copy_87))
        tx.define(lib.relation_slot(copy_42, "location"), Fact(copy_42, Predicate("located_at"), shelf_a3))
        tx.define(lib.relation_slot(copy_87, "location"), Fact(copy_87, Predicate("located_at"), shelf_b1))

    who_wrote = db.compile_pattern(lib.NS, Symbol("Foundation"), "written_by", Variable("author"))
    copies_and_shelves = [
        db.compile_pattern(lib.NS, Symbol("Foundation"), "has_copy", Variable("copy")),
        Pattern(Variable("copy"), Predicate("located_at"), Variable("shelf")),
    ]

    author_result = db.query([who_wrote])
    location_result = db.query(copies_and_shelves)

    # Checkout is a single committed fact.
    borrower_slot = lib.relation_slot(copy_42, "borrower")
    with db.transaction() as tx:
        tx.define(borrower_slot, Fact(copy_42, Predicate("borrowed_by"), alice))

    alice_holdings = [
        Pattern(Variable("copy"), Predicate("borrowed_by"), alice),
        Pattern(Variable("work"), Predicate("has_copy"), Variable("copy")),
    ]
    holdings_before_rename = db.query(alice_holdings)

    # Capture the old world before moving Copy 87.
    before_move = db.snapshot()
    with db.transaction() as tx:
        tx.define(lib.relation_slot(copy_87, "location"), Fact(copy_87, Predicate("located_at"), shelf_c3))
    moved = db.query([Pattern(copy_87, Predicate("located_at"), Variable("shelf"))])
    old_world_location = before_move.query([Pattern(copy_87, Predicate("located_at"), Variable("shelf"))])

    compiled_alice = db.compile_pattern(lib.NS, Variable("copy"), "borrowed_by", Symbol("Alice"))

    # Rename and symbol rebinding happen atomically together, yet the compiled query
    # continues to point at Alice's stable identity.
    with db.transaction() as tx:
        db.define_display_name(tx, alice, "Alicia")
        db.bind_symbol(tx, lib.NS, Symbol("Alice"), bob)

    old_compiled_result = db.query([compiled_alice])
    newly_compiled_alice = db.compile_pattern(lib.NS, Variable("copy"), "borrowed_by", Symbol("Alice"))
    new_compiled_result = db.query([newly_compiled_alice])

    with db.transaction() as tx:
        tx.forget(borrower_slot)
    after_return = db.query([Pattern(copy_42, Predicate("borrowed_by"), Variable("patron"))])

    explained = db.query(copies_and_shelves)
    plan = db.explain(explained)

    pre_restart_version = db.version
    pre_restart_digest = db.digest
    recovered = CommittedWorldDB.open(path)
    recovered_locations = recovered.query(copies_and_shelves)

    return {
        "author": [db.render_binding(row.binding) for row in author_result.rows],
        "copies_and_shelves_initial": [db.render_binding(row.binding) for row in location_result.rows],
        "alice_holdings": [db.render_binding(row.binding) for row in holdings_before_rename.rows],
        "copy_87_before_move_snapshot": [before_move.render_binding(row.binding) for row in old_world_location.rows],
        "copy_87_after_move": [db.render_binding(row.binding) for row in moved.rows],
        "old_compiled_after_atomic_rename_and_rebind": [db.render_binding(row.binding) for row in old_compiled_result.rows],
        "new_compiled_after_symbol_rebind": [db.render_binding(row.binding) for row in new_compiled_result.rows],
        "after_return": [db.render_binding(row.binding) for row in after_return.rows],
        "query_plan": plan,
        "world_version": db.version,
        "world_digest": db.digest,
        "active_slots": db.active_slots,
        "immutable_records": db.immutable_records,
        "recovery": {
            "same_version": recovered.version == pre_restart_version,
            "same_digest": recovered.digest == pre_restart_digest,
            "copies_and_shelves": [recovered.render_binding(row.binding) for row in recovered_locations.rows],
        },
    }


class CommittedWorldTests(unittest.TestCase):
    def test_library_recovery_matches_live_world(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            report = run_library(Path(directory) / "library.fdb")
            self.assertTrue(report["recovery"]["same_version"])
            self.assertTrue(report["recovery"]["same_digest"])
            self.assertEqual(report["after_return"], [])
            self.assertEqual(report["copy_87_before_move_snapshot"], [{"shelf": "Shelf B1"}])
            self.assertEqual(report["copy_87_after_move"], [{"shelf": "Shelf C3"}])
            self.assertEqual(report["old_compiled_after_atomic_rename_and_rebind"], [{"copy": "Copy 42"}])
            self.assertEqual(report["new_compiled_after_symbol_rebind"], [])

    def test_stale_writer_aborts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "stale.fdb"
            db = CommittedWorldDB.open(path)
            first = db.transaction()
            second = db.transaction()
            first.define(SlotId("a"), Fact(Literal("a"), Predicate("is"), Literal("one")))
            second.define(SlotId("b"), Fact(Literal("b"), Predicate("is"), Literal("two")))
            first.commit()
            with self.assertRaises(TransactionConflict):
                second.commit()
            self.assertIsNone(db.resolve(SlotId("b")))

    def test_incomplete_tail_is_not_a_world(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "tail.fdb"
            db = CommittedWorldDB.open(path)
            with db.transaction() as tx:
                tx.define(SlotId("stable"), Fact(Literal("x"), Predicate("state"), Literal("committed")))
            expected_version = db.version
            expected_digest = db.digest
            with open(path, "ab") as log_file:
                log_file.write(b"FDB1\x00\x00\x00")
                log_file.flush()
                os.fsync(log_file.fileno())
            recovered = CommittedWorldDB.open(path)
            self.assertEqual(recovered.version, expected_version)
            self.assertEqual(recovered.digest, expected_digest)

    def test_durable_commit_recovers_after_failure_before_publish(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "crash.fdb"
            db = CommittedWorldDB.open(path)
            tx = db.transaction()
            tx.define(SlotId("durable"), Fact(Literal("x"), Predicate("state"), Literal("yes")))
            with self.assertRaises(RuntimeError):
                tx.commit(fail_after_fsync=True)
            # This process object did not publish the world.
            self.assertIsNone(db.resolve(SlotId("durable")))
            # A restarted process recovers the fsynced frame.
            recovered = CommittedWorldDB.open(path)
            self.assertEqual(
                recovered.resolve(SlotId("durable")),
                Fact(Literal("x"), Predicate("state"), Literal("yes")),
            )

    def test_transaction_reads_its_candidate_world(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "candidate.fdb"
            db = CommittedWorldDB.open(path)
            tx = db.transaction()
            entity = tx.entity()
            tx.define(SlotId("entity/name"), Fact(entity, Predicate("name"), Literal("Candidate")))
            self.assertEqual(
                tx.query([Pattern(entity, Predicate("name"), Variable("name"))]).bindings(),
                [{"name": Literal("Candidate")}],
            )
            self.assertEqual(db.query([Pattern(entity, Predicate("name"), Variable("name"))]).rows, ())
            tx.commit()
            self.assertEqual(
                db.query([Pattern(entity, Predicate("name"), Variable("name"))]).bindings(),
                [{"name": Literal("Candidate")}],
            )

    def test_constraint_rejection_creates_no_world(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "constraint.fdb"
            db = CommittedWorldDB.open(path)
            original_size = path.stat().st_size
            tx = db.transaction()
            tx.define(SlotId("account/balance"), Fact(Literal("account"), Predicate("balance"), Literal("-1")))

            def reject_negative(candidate) -> None:
                fact = candidate.resolve(SlotId("account/balance"))
                if fact is not None and fact.object == Literal("-1"):
                    raise ValueError("negative balance")

            tx.require(reject_negative)
            with self.assertRaises(Exception):
                tx.commit()
            self.assertEqual(db.version, 0)
            self.assertEqual(path.stat().st_size, original_size)
            self.assertIsNone(db.resolve(SlotId("account/balance")))

    def test_validator_cannot_mutate_candidate(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "validator-mutation.fdb"
            db = CommittedWorldDB.open(path)
            tx = db.transaction()
            tx.define(SlotId("safe"), Fact(Literal("safe"), Predicate("state"), Literal("yes")))

            def mutate(candidate) -> None:
                candidate.define(SlotId("hidden"), Fact(Literal("hidden"), Predicate("state"), Literal("bad")))

            tx.require(mutate)
            with self.assertRaises(Exception):
                tx.commit()
            self.assertEqual(db.version, 0)
            self.assertIsNone(db.resolve(SlotId("safe")))
            self.assertIsNone(db.resolve(SlotId("hidden")))

    def test_mid_log_corruption_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "corrupt.fdb"
            db = CommittedWorldDB.open(path)
            with db.transaction() as tx:
                tx.define(SlotId("a"), Fact(Literal("a"), Predicate("state"), Literal("1")))
            with db.transaction() as tx:
                tx.define(SlotId("b"), Fact(Literal("b"), Predicate("state"), Literal("2")))
            data = bytearray(path.read_bytes())
            # Flip a byte in the first payload, leaving a later frame after it.
            data[20] ^= 0x01
            path.write_bytes(data)
            with self.assertRaises(LogCorruption):
                CommittedWorldDB.open(path)


if __name__ == "__main__":
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(CommittedWorldTests)
    outcome = unittest.TextTestRunner(verbosity=2).run(suite)
    if not outcome.wasSuccessful():
        raise SystemExit(1)

    with tempfile.TemporaryDirectory() as directory:
        report = run_library(Path(directory) / "library.fdb")
        print("\n" + "=" * 78)
        print("COMMITTED-WORLD LIBRARY HARNESS")
        print("=" * 78)
        print(json.dumps(report, indent=2, sort_keys=True))
