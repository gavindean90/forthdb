from __future__ import annotations

from dataclasses import dataclass
from typing import List, Optional, Tuple

from forthdb_kernel import (
    CurrentView,
    DefinitionStore,
    Fact,
    ForthDB,
    QueryExecutor,
    Record,
    RecordId,
    SlotId,
)


@dataclass(frozen=True)
class DefineOp:
    slot: SlotId
    fact: Fact


@dataclass(frozen=True)
class ForgetOp:
    slot: SlotId


Operation = DefineOp | ForgetOp


class TransactionClosedError(RuntimeError):
    pass


class AtomicTransaction:
    """A private batch of writes that becomes visible at one publish point.

    This is an executable model of atomicity, not a durable disk transaction.
    Operations are staged without touching the live database. Commit copies the
    current logical state, applies the operations to the copy, validates it, and
    then publishes the completed store and indexes with one facade-level swap.
    """

    def __init__(self, db: "AtomicForthDB") -> None:
        self._db = db
        self._operations: List[Operation] = []
        self._closed = False

    @property
    def operations(self) -> Tuple[Operation, ...]:
        return tuple(self._operations)

    def _require_open(self) -> None:
        if self._closed:
            raise TransactionClosedError("Transaction is already closed")

    def define(self, slot: SlotId, fact: Fact) -> "AtomicTransaction":
        self._require_open()
        self._operations.append(DefineOp(slot, fact))
        return self

    def forget(self, slot: SlotId) -> "AtomicTransaction":
        self._require_open()
        self._operations.append(ForgetOp(slot))
        return self

    def rollback(self) -> None:
        self._require_open()
        self._operations.clear()
        self._closed = True

    def commit(self, *, fail_before_publish: bool = False) -> Tuple[Record, ...]:
        self._require_open()
        try:
            records = self._db._commit_operations(
                tuple(self._operations),
                fail_before_publish=fail_before_publish,
            )
        finally:
            self._closed = True
        return records

    def __enter__(self) -> "AtomicTransaction":
        self._require_open()
        return self

    def __exit__(self, exc_type, exc, traceback) -> bool:
        if exc_type is not None:
            self.rollback()
            return False
        self.commit()
        return False


class AtomicForthDB(ForthDB):
    """ForthDB with model transactions and atomic batch publication.

    Atomicity guarantee for this in-memory model:
      * staged writes are invisible before commit;
      * a failed commit leaves the live state unchanged;
      * a successful commit publishes every staged operation together.

    This does not yet claim crash durability, concurrency control, or isolation
    between simultaneous writers. Those belong to a storage-engine experiment.
    """

    def transaction(self) -> AtomicTransaction:
        return AtomicTransaction(self)

    def _clone(self) -> ForthDB:
        clone = ForthDB()

        # Records and typed values are immutable, so copying the containers is
        # sufficient to produce a private logical state.
        clone.store.log = list(self.store.log)
        clone.store.head = dict(self.store.head)
        clone.store.slot_history.clear()
        for slot, record_ids in self.store.slot_history.items():
            clone.store.slot_history[slot] = list(record_ids)

        clone._next_entity = self._next_entity

        # Rebuild derived indexes from the copied heads. This deliberately
        # treats the immutable log + head map as authoritative state.
        clone.view = CurrentView(clone.store)
        for record_id in clone.store.head.values():
            clone.view.add(record_id)
        clone.executor = QueryExecutor(clone.store, clone.view)
        return clone

    def _commit_operations(
        self,
        operations: Tuple[Operation, ...],
        *,
        fail_before_publish: bool = False,
    ) -> Tuple[Record, ...]:
        shadow = self._clone()
        committed: List[Record] = []

        for operation in operations:
            if isinstance(operation, DefineOp):
                committed.append(shadow.define(operation.slot, operation.fact))
            elif isinstance(operation, ForgetOp):
                committed.append(shadow.forget(operation.slot))
            else:
                raise TypeError(operation)

        shadow.validate()

        if fail_before_publish:
            raise RuntimeError("Injected failure before atomic publish")

        # Commit point: after this facade-level publication, all reads use the
        # complete new store and matching indexes. Before it, all reads used the
        # complete old state.
        self.store = shadow.store
        self.view = shadow.view
        self.executor = QueryExecutor(self.store, self.view)
        self._next_entity = shadow._next_entity

        return tuple(committed)

    def atomic_define(self, slot: SlotId, fact: Fact) -> Record:
        return self.transaction().define(slot, fact).commit()[0]

    def atomic_forget(self, slot: SlotId) -> Record:
        return self.transaction().forget(slot).commit()[0]
