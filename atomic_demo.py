from __future__ import annotations

import unittest

from forthdb_atomic import AtomicForthDB, TransactionClosedError
from forthdb_kernel import Fact, Literal, Pattern, Predicate, SlotId, Variable
from library_demo import KernelRegressionTests, run_library


class AtomicKernelTests(unittest.TestCase):
    def test_staged_writes_are_invisible_until_commit(self) -> None:
        db = AtomicForthDB()
        account = db.entity()
        balance = SlotId("account/1/balance")

        tx = db.transaction()
        tx.define(balance, Fact(account, Predicate("balance"), Literal("100")))

        self.assertIsNone(db.resolve(balance))
        self.assertEqual(
            db.query([Pattern(account, Predicate("balance"), Variable("amount"))]).rows,
            (),
        )

        tx.commit()
        self.assertEqual(db.resolve(balance).object, Literal("100"))

    def test_batch_commit_has_one_visibility_boundary(self) -> None:
        db = AtomicForthDB()
        account_a = db.entity()
        account_b = db.entity()
        a = SlotId("account/a/balance")
        b = SlotId("account/b/balance")
        balance = Predicate("balance")

        db.define(a, Fact(account_a, balance, Literal("100")))
        db.define(b, Fact(account_b, balance, Literal("100")))

        tx = db.transaction()
        tx.define(a, Fact(account_a, balance, Literal("75")))
        tx.define(b, Fact(account_b, balance, Literal("125")))

        self.assertEqual(db.resolve(a).object, Literal("100"))
        self.assertEqual(db.resolve(b).object, Literal("100"))

        tx.commit()

        self.assertEqual(db.resolve(a).object, Literal("75"))
        self.assertEqual(db.resolve(b).object, Literal("125"))
        self.assertEqual(len(db.history(a)), 2)
        self.assertEqual(len(db.history(b)), 2)
        db.validate()

    def test_failed_commit_leaves_live_state_unchanged(self) -> None:
        db = AtomicForthDB()
        entity = db.entity()
        state = SlotId("entity/state")
        predicate = Predicate("state")
        db.define(state, Fact(entity, predicate, Literal("old")))

        before_log = tuple(db.store.log)
        before_head = dict(db.store.head)
        before_fact = db.resolve(state)

        tx = db.transaction()
        tx.define(state, Fact(entity, predicate, Literal("new")))
        tx.define(SlotId("entity/other"), Fact(entity, Predicate("other"), Literal("x")))

        with self.assertRaisesRegex(RuntimeError, "Injected failure"):
            tx.commit(fail_before_publish=True)

        self.assertEqual(tuple(db.store.log), before_log)
        self.assertEqual(db.store.head, before_head)
        self.assertEqual(db.resolve(state), before_fact)
        self.assertIsNone(db.resolve(SlotId("entity/other")))
        db.validate()

    def test_exception_rolls_back_context_manager(self) -> None:
        db = AtomicForthDB()
        entity = db.entity()
        slot = SlotId("context/value")

        with self.assertRaisesRegex(ValueError, "abort"):
            with db.transaction() as tx:
                tx.define(slot, Fact(entity, Predicate("value"), Literal("hidden")))
                raise ValueError("abort")

        self.assertIsNone(db.resolve(slot))

    def test_order_within_transaction_is_preserved(self) -> None:
        db = AtomicForthDB()
        entity = db.entity()
        slot = SlotId("ordered/state")
        predicate = Predicate("state")

        with db.transaction() as tx:
            tx.define(slot, Fact(entity, predicate, Literal("v1")))
            tx.define(slot, Fact(entity, predicate, Literal("v2")))
            tx.forget(slot)

        self.assertEqual(db.resolve(slot).object, Literal("v1"))
        self.assertEqual([record.kind for record in db.history(slot)], ["define", "define", "forget"])

    def test_closed_transaction_rejects_reuse(self) -> None:
        db = AtomicForthDB()
        tx = db.transaction()
        tx.commit()
        with self.assertRaises(TransactionClosedError):
            tx.commit()


if __name__ == "__main__":
    suite = unittest.TestSuite()
    suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(KernelRegressionTests))
    suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(AtomicKernelTests))
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    if not result.wasSuccessful():
        raise SystemExit(1)

    # Confirm the original application remains reproducible with the untouched
    # kernel while the atomic model evolves beside it.
    report = run_library()
    print("\nAtomic model complete")
    print(f"Original library active slots: {report['active_slots']}")
    print(f"Original library immutable records: {report['immutable_records']}")
