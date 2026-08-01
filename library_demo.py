
from __future__ import annotations

import json
import unittest

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


class KernelRegressionTests(unittest.TestCase):
    def test_cumulative_kernel(self) -> None:
        db = ForthDB()

        work = db.entity()
        copy_1 = db.entity()
        copy_2 = db.entity()
        shelf_a = db.entity()
        shelf_b = db.entity()

        db.define(SlotId("work/copy/1"), Fact(work, Predicate("has_copy"), copy_1))
        db.define(SlotId("work/copy/2"), Fact(work, Predicate("has_copy"), copy_2))
        db.define(SlotId("copy/1/location"), Fact(copy_1, Predicate("located_at"), shelf_a))
        db.define(SlotId("copy/2/location"), Fact(copy_2, Predicate("located_at"), shelf_b))

        traversal = db.query(
            [
                Pattern(work, Predicate("has_copy"), Variable("copy")),
                Pattern(Variable("copy"), Predicate("located_at"), Variable("shelf")),
            ]
        )
        self.assertEqual(len(traversal.rows), 2)

        db.define(SlotId("copy/1/location"), Fact(copy_1, Predicate("located_at"), shelf_b))
        current = db.query([Pattern(copy_1, Predicate("located_at"), Variable("shelf"))])
        self.assertEqual(current.bindings(), [{"shelf": shelf_b}])
        self.assertEqual(len(db.definitions(SlotId("copy/1/location"))), 2)

        db.forget(SlotId("copy/1/location"))
        restored = db.query([Pattern(copy_1, Predicate("located_at"), Variable("shelf"))])
        self.assertEqual(restored.bindings(), [{"shelf": shelf_a}])

        db.define(SlotId("dup/a"), Fact(work, Predicate("has_copy"), copy_1))
        db.define(SlotId("dup/b"), Fact(work, Predicate("has_copy"), copy_1))

        distinct = db.query([Pattern(work, Predicate("has_copy"), copy_1)])
        all_rows = db.query(
            [Pattern(work, Predicate("has_copy"), copy_1)],
            distinct=False,
            include_provenance=True,
        )
        self.assertEqual(len(distinct.rows), 1)
        self.assertEqual(len(all_rows.rows), 3)
        self.assertTrue(all(row.provenance for row in all_rows.rows))

        deep = SlotId("deep/state")
        db.define(deep, Fact(Literal("deep"), Predicate("state"), Literal("v0")))
        for index in range(1, 1001):
            db.define(deep, Fact(Literal("deep"), Predicate("state"), Literal(f"v{index}")))

        deep_result = db.query(
            [Pattern(Literal("deep"), Predicate("state"), Variable("value"))]
        )
        self.assertEqual(deep_result.bindings(), [{"value": Literal("v1000")}])
        self.assertEqual(deep_result.metrics.candidate_facts, 1)

        db.validate()

    def test_compiled_identity(self) -> None:
        db = ForthDB()
        john = db.entity()
        bob = db.entity()
        other = db.entity()

        db.define_display_name(john, "John")
        db.define_display_name(bob, "Bob")
        db.define_display_name(other, "Other Bob")

        db.bind_symbol("global", Symbol("John"), john)
        db.bind_symbol("global", Symbol("Bob"), bob)
        db.define(SlotId("relationship/john-bob"), Fact(john, Predicate("friend"), bob))

        compiled = db.compile_pattern("global", Symbol("John"), "friend", Symbol("Bob"))
        self.assertEqual(compiled, Pattern(john, Predicate("friend"), bob))

        db.bind_symbol("global", Symbol("Bob"), other)
        new_compiled = db.compile_pattern("global", Symbol("John"), "friend", Symbol("Bob"))

        self.assertEqual(compiled, Pattern(john, Predicate("friend"), bob))
        self.assertEqual(new_compiled, Pattern(john, Predicate("friend"), other))
        self.assertEqual(len(db.query([compiled]).rows), 1)
        self.assertEqual(len(db.query([new_compiled]).rows), 0)

        db.define_display_name(bob, "Robert")
        self.assertEqual(
            db.render_binding({"friend": bob}),
            {"friend": "Robert"},
        )


class LibraryHarness:
    NS = "library"

    def __init__(self) -> None:
        self.db = ForthDB()

    def named(self, symbol: str, display: str | None = None) -> EntityId:
        entity = self.db.entity()
        self.db.define_display_name(entity, display or symbol.replace("_", " "))
        self.db.bind_symbol(self.NS, Symbol(symbol), entity)
        return entity

    @staticmethod
    def relation_slot(owner: EntityId, relation: str, suffix: str = "current") -> SlotId:
        return SlotId(f"{owner.value}/{relation}/{suffix}")

    def add(self, slot: SlotId, subject: EntityId, predicate: str, object_: EntityId) -> None:
        self.db.define(slot, Fact(subject, Predicate(predicate), object_))


def run_library() -> dict:
    lib = LibraryHarness()
    db = lib.db

    asimov = lib.named("Asimov", "Isaac Asimov")
    foundation = lib.named("Foundation")
    science_fiction = lib.named("Science_Fiction", "Science Fiction")
    copy_42 = lib.named("Copy_42", "Copy 42")
    copy_87 = lib.named("Copy_87", "Copy 87")
    shelf_a3 = lib.named("Shelf_A3", "Shelf A3")
    shelf_b1 = lib.named("Shelf_B1", "Shelf B1")
    shelf_c3 = lib.named("Shelf_C3", "Shelf C3")
    alice = lib.named("Alice")
    bob = lib.named("Bob")

    lib.add(lib.relation_slot(foundation, "author"), foundation, "written_by", asimov)
    lib.add(lib.relation_slot(foundation, "subject", "sf"), foundation, "subject", science_fiction)
    lib.add(lib.relation_slot(foundation, "copy", "42"), foundation, "has_copy", copy_42)
    lib.add(lib.relation_slot(foundation, "copy", "87"), foundation, "has_copy", copy_87)
    lib.add(lib.relation_slot(copy_42, "location"), copy_42, "located_at", shelf_a3)
    lib.add(lib.relation_slot(copy_87, "location"), copy_87, "located_at", shelf_b1)

    # Compile once through the human-facing namespace.
    who_wrote = db.compile_pattern(
        lib.NS,
        Symbol("Foundation"),
        "written_by",
        Variable("author"),
    )

    copies_and_shelves = [
        db.compile_pattern(lib.NS, Symbol("Foundation"), "has_copy", Variable("copy")),
        Pattern(Variable("copy"), Predicate("located_at"), Variable("shelf")),
    ]

    author_result = db.query([who_wrote])
    location_result = db.query(copies_and_shelves)

    # Checkout Copy 42 to Alice.
    borrower_slot = lib.relation_slot(copy_42, "borrower")
    lib.add(borrower_slot, copy_42, "borrowed_by", alice)

    alice_holdings = [
        Pattern(Variable("copy"), Predicate("borrowed_by"), alice),
        Pattern(Variable("work"), Predicate("has_copy"), Variable("copy")),
    ]
    holdings_before_rename = db.query(alice_holdings)

    # Move Copy 87 by redefining the same location slot.
    lib.add(lib.relation_slot(copy_87, "location"), copy_87, "located_at", shelf_c3)
    moved = db.query([Pattern(copy_87, Predicate("located_at"), Variable("shelf"))])

    # Compile a patron query before renaming or rebinding.
    compiled_alice = db.compile_pattern(
        lib.NS,
        Variable("copy"),
        "borrowed_by",
        Symbol("Alice"),
    )

    db.define_display_name(alice, "Alicia")
    compiled_alice_after_rename = db.query([compiled_alice])

    # Rebind the source symbol Alice to Bob. Existing compiled query must not change.
    db.bind_symbol(lib.NS, Symbol("Alice"), bob)
    newly_compiled_alice = db.compile_pattern(
        lib.NS,
        Variable("copy"),
        "borrowed_by",
        Symbol("Alice"),
    )

    old_compiled_result = db.query([compiled_alice])
    new_compiled_result = db.query([newly_compiled_alice])

    # Return Copy 42.
    db.forget(borrower_slot)
    after_return = db.query([Pattern(copy_42, Predicate("borrowed_by"), Variable("patron"))])

    # Explain the most interesting traversal.
    explained = db.query(copies_and_shelves)
    plan = db.explain(explained)

    # The current kernel cannot undo a forget that empties a slot without
    # explicitly defining the hidden fact again. Preserve this as an earned gap.
    return_gap = {
        "slot_current": db.resolve(borrower_slot),
        "history": [
            {
                "kind": record.kind,
                "fact": None if record.fact is None else str(record.fact),
            }
            for record in db.history(borrower_slot)
        ],
        "needs_restore_primitive": True,
    }

    db.validate()

    return {
        "author": [db.render_binding(row.binding) for row in author_result.rows],
        "copies_and_shelves_initial": [
            db.render_binding(row.binding) for row in location_result.rows
        ],
        "alice_holdings": [
            db.render_binding(row.binding) for row in holdings_before_rename.rows
        ],
        "copy_87_after_move": [
            db.render_binding(row.binding) for row in moved.rows
        ],
        "old_compiled_after_rename": [
            db.render_binding(row.binding) for row in compiled_alice_after_rename.rows
        ],
        "old_compiled_after_symbol_rebind": [
            db.render_binding(row.binding) for row in old_compiled_result.rows
        ],
        "new_compiled_after_symbol_rebind": [
            db.render_binding(row.binding) for row in new_compiled_result.rows
        ],
        "after_return": [
            db.render_binding(row.binding) for row in after_return.rows
        ],
        "query_plan": plan,
        "return_gap": return_gap,
        "active_slots": len(db.store.head),
        "immutable_records": len(db.store.log),
    }


if __name__ == "__main__":
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(KernelRegressionTests)
    outcome = unittest.TextTestRunner(verbosity=2).run(suite)
    if not outcome.wasSuccessful():
        raise SystemExit(1)

    report = run_library()
    print("\n" + "=" * 78)
    print("LIBRARY HARNESS")
    print("=" * 78)
    print(json.dumps(report, indent=2, sort_keys=True))
