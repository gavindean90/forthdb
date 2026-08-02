from __future__ import annotations

import hashlib
import json
import os
import struct
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterable, List, Mapping, Optional, Sequence, Tuple, Union

from forthdb_kernel import (
    Atom,
    BoundValue,
    CurrentView,
    EntityId,
    Fact,
    ForthDB,
    Literal,
    Pattern,
    Predicate,
    QueryExecutor,
    QueryResult,
    Record,
    SlotId,
    SourceTerm,
    Symbol,
    Variable,
)


MAGIC = b"FDB1"
FORMAT_VERSION = 1
HEADER = struct.Struct(">4sQ")          # magic, payload length
TRAILER = struct.Struct(">32sQ")       # sha256(payload), repeated length
GENESIS_DIGEST = hashlib.sha256(b"forthdb committed-world genesis v1").hexdigest()


class TransactionClosedError(RuntimeError):
    pass


class TransactionConflict(RuntimeError):
    pass


class ConstraintViolation(RuntimeError):
    pass


class LogCorruption(RuntimeError):
    pass


@dataclass(frozen=True)
class DefineOp:
    slot: SlotId
    fact: Fact


@dataclass(frozen=True)
class ForgetOp:
    slot: SlotId


Operation = Union[DefineOp, ForgetOp]
Validator = Callable[[ForthDB], None]


@dataclass(frozen=True)
class CommittedWorld:
    """One immutable, materialized interpretation of the commit chain.

    The wrapped ForthDB instance is private and is never mutated after the world
    is published. Queries capture this object once and therefore remain on one
    stable world even if a later commit is published.
    """

    version: int
    digest: str
    kernel: ForthDB


@dataclass(frozen=True)
class CommitReceipt:
    version: int
    parent_digest: str
    world_digest: str
    operation_count: int
    records: Tuple[Record, ...]


def _clone_kernel(source: ForthDB) -> ForthDB:
    clone = ForthDB()
    clone.store.log = list(source.store.log)
    clone.store.head = dict(source.store.head)
    clone.store.slot_history.clear()
    for slot, record_ids in source.store.slot_history.items():
        clone.store.slot_history[slot] = list(record_ids)
    clone._next_entity = source._next_entity

    clone.view = CurrentView(clone.store)
    for record_id in clone.store.head.values():
        clone.view.add(record_id)
    clone.executor = QueryExecutor(clone.store, clone.view)
    return clone


def _encode_atom(value: Atom) -> Mapping[str, object]:
    if isinstance(value, EntityId):
        return {"type": "entity", "value": value.value}
    if isinstance(value, Literal):
        return {"type": "literal", "value": value.value}
    raise TypeError(value)


def _decode_atom(value: Mapping[str, object]) -> Atom:
    kind = value.get("type")
    raw = value.get("value")
    if kind == "entity" and isinstance(raw, int):
        return EntityId(raw)
    if kind == "literal" and isinstance(raw, str):
        return Literal(raw)
    raise LogCorruption(f"Invalid atom: {value!r}")


def _encode_fact(fact: Fact) -> Mapping[str, object]:
    return {
        "subject": _encode_atom(fact.subject),
        "predicate": fact.predicate.name,
        "object": _encode_atom(fact.object),
    }


def _decode_fact(value: Mapping[str, object]) -> Fact:
    subject = value.get("subject")
    predicate = value.get("predicate")
    object_ = value.get("object")
    if not isinstance(subject, Mapping) or not isinstance(predicate, str) or not isinstance(object_, Mapping):
        raise LogCorruption(f"Invalid fact: {value!r}")
    return Fact(_decode_atom(subject), Predicate(predicate), _decode_atom(object_))


def _encode_operation(operation: Operation, *, version: int, ordinal: int) -> Mapping[str, object]:
    if isinstance(operation, DefineOp):
        return {
            "kind": "define",
            "slot": operation.slot.value,
            "definition_id": f"{version}:{ordinal}",
            "fact": _encode_fact(operation.fact),
        }
    if isinstance(operation, ForgetOp):
        return {"kind": "forget", "slot": operation.slot.value}
    raise TypeError(operation)


def _decode_operation(value: Mapping[str, object]) -> Operation:
    kind = value.get("kind")
    slot = value.get("slot")
    if not isinstance(slot, str):
        raise LogCorruption(f"Invalid operation slot: {value!r}")
    if kind == "define":
        fact = value.get("fact")
        if not isinstance(fact, Mapping):
            raise LogCorruption(f"Invalid define operation: {value!r}")
        return DefineOp(SlotId(slot), _decode_fact(fact))
    if kind == "forget":
        return ForgetOp(SlotId(slot))
    raise LogCorruption(f"Invalid operation kind: {value!r}")


def _canonical_bytes(value: object) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def _world_projection(kernel: ForthDB) -> Mapping[str, object]:
    """Canonical authoritative-state projection used for deterministic recovery checks."""

    records: List[Mapping[str, object]] = []
    for record in kernel.store.log:
        records.append(
            {
                "id": record.id.value,
                "kind": record.kind,
                "slot": record.slot.value,
                "fact": None if record.fact is None else _encode_fact(record.fact),
                "previous_head": None if record.previous_head is None else record.previous_head.value,
                "resulting_head": None if record.resulting_head is None else record.resulting_head.value,
            }
        )

    heads = {
        slot.value: record_id.value
        for slot, record_id in sorted(kernel.store.head.items(), key=lambda item: item[0].value)
    }

    return {
        "next_entity": kernel._next_entity,
        "records": records,
        "heads": heads,
    }


def _world_digest(kernel: ForthDB) -> str:
    return hashlib.sha256(_canonical_bytes(_world_projection(kernel))).hexdigest()


def _apply_operations(kernel: ForthDB, operations: Iterable[Operation]) -> Tuple[Record, ...]:
    records: List[Record] = []
    for operation in operations:
        if isinstance(operation, DefineOp):
            records.append(kernel.define(operation.slot, operation.fact))
        elif isinstance(operation, ForgetOp):
            records.append(kernel.forget(operation.slot))
        else:
            raise TypeError(operation)
    return tuple(records)


def _frame_bytes(payload: bytes) -> bytes:
    checksum = hashlib.sha256(payload).digest()
    return HEADER.pack(MAGIC, len(payload)) + payload + TRAILER.pack(checksum, len(payload))


class WorldSnapshot:
    """Read-only facade pinned to one committed world."""

    def __init__(self, world: CommittedWorld) -> None:
        self._world = world

    @property
    def version(self) -> int:
        return self._world.version

    @property
    def digest(self) -> str:
        return self._world.digest

    def query(self, patterns: Sequence[Pattern], **kwargs: object) -> QueryResult:
        return self._world.kernel.query(patterns, **kwargs)

    def resolve(self, slot: SlotId) -> Optional[Fact]:
        return self._world.kernel.resolve(slot)

    def definitions(self, slot: SlotId) -> Tuple[Fact, ...]:
        return self._world.kernel.definitions(slot)

    def history(self, slot: SlotId) -> Tuple[Record, ...]:
        return self._world.kernel.history(slot)

    def compile_pattern(
        self,
        namespace: str,
        subject: SourceTerm,
        predicate: Union[str, Predicate],
        object_: SourceTerm,
    ) -> Pattern:
        return self._world.kernel.compile_pattern(namespace, subject, predicate, object_)

    def render_binding(self, binding: Mapping[str, BoundValue]) -> Mapping[str, str]:
        return self._world.kernel.render_binding(binding)

    def explain(self, result: QueryResult) -> Mapping[str, object]:
        return self._world.kernel.explain(result)


class WorldTransaction:
    """Private candidate successor to exactly one committed world."""

    def __init__(self, db: "CommittedWorldDB", base: CommittedWorld) -> None:
        self._db = db
        self._base = base
        self._operations: List[Operation] = []
        self._validators: List[Validator] = []
        self._next_entity = base.kernel._next_entity
        self._closed = False

    @property
    def base_version(self) -> int:
        return self._base.version

    @property
    def operations(self) -> Tuple[Operation, ...]:
        return tuple(self._operations)

    def _require_open(self) -> None:
        if self._closed:
            raise TransactionClosedError("Transaction is already closed")

    def entity(self) -> EntityId:
        self._require_open()
        entity = EntityId(self._next_entity)
        self._next_entity += 1
        return entity

    def define(self, slot: SlotId, fact: Fact) -> "WorldTransaction":
        self._require_open()
        self._operations.append(DefineOp(slot, fact))
        return self

    def forget(self, slot: SlotId) -> "WorldTransaction":
        self._require_open()
        self._operations.append(ForgetOp(slot))
        return self

    def require(self, validator: Validator) -> "WorldTransaction":
        self._require_open()
        self._validators.append(validator)
        return self

    def _candidate(self) -> Tuple[ForthDB, Tuple[Record, ...]]:
        candidate = _clone_kernel(self._base.kernel)
        candidate._next_entity = self._next_entity
        records = _apply_operations(candidate, self._operations)
        candidate.validate()
        pre_validation_digest = _world_digest(candidate)
        for validator in self._validators:
            try:
                validator(candidate)
            except Exception as exc:
                raise ConstraintViolation(str(exc)) from exc
        candidate.validate()
        if _world_digest(candidate) != pre_validation_digest:
            raise ConstraintViolation("Constraint validators must be read-only")
        return candidate, records

    def snapshot(self) -> WorldSnapshot:
        self._require_open()
        candidate, _ = self._candidate()
        world = CommittedWorld(self._base.version + 1, _world_digest(candidate), candidate)
        return WorldSnapshot(world)

    def query(self, patterns: Sequence[Pattern], **kwargs: object) -> QueryResult:
        return self.snapshot().query(patterns, **kwargs)

    def rollback(self) -> None:
        self._require_open()
        self._operations.clear()
        self._validators.clear()
        self._closed = True

    def commit(self, *, fail_after_fsync: bool = False) -> CommitReceipt:
        self._require_open()
        try:
            return self._db._commit(self, fail_after_fsync=fail_after_fsync)
        finally:
            self._closed = True

    def __enter__(self) -> "WorldTransaction":
        self._require_open()
        return self

    def __exit__(self, exc_type, exc, traceback) -> bool:
        if exc_type is not None:
            self.rollback()
            return False
        self.commit()
        return False


class CommittedWorldDB:
    """Durable committed-world model for ForthDB.

    Contract:
      * each transaction is one complete checksummed append frame;
      * a commit extends exactly one parent world;
      * stale writers abort;
      * the frame is fsynced before the new world is published;
      * recovery replays complete valid frames and ignores an incomplete tail;
      * indexes are derived and rebuilt, never authoritative.

    This model serializes writers inside one process. Cross-process writer
    exclusion is intentionally outside this experiment.
    """

    def __init__(self, path: Union[str, os.PathLike[str]]) -> None:
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self._commit_lock = threading.Lock()
        self._world = self._recover()

    @classmethod
    def open(cls, path: Union[str, os.PathLike[str]]) -> "CommittedWorldDB":
        return cls(path)

    @property
    def version(self) -> int:
        return self._world.version

    @property
    def digest(self) -> str:
        return self._world.digest

    @property
    def active_slots(self) -> int:
        return len(self._world.kernel.store.head)

    @property
    def immutable_records(self) -> int:
        return len(self._world.kernel.store.log)

    def snapshot(self) -> WorldSnapshot:
        world = self._world
        return WorldSnapshot(world)

    def transaction(self) -> WorldTransaction:
        base = self._world
        return WorldTransaction(self, base)

    def query(self, patterns: Sequence[Pattern], **kwargs: object) -> QueryResult:
        world = self._world
        return world.kernel.query(patterns, **kwargs)

    def resolve(self, slot: SlotId) -> Optional[Fact]:
        world = self._world
        return world.kernel.resolve(slot)

    def definitions(self, slot: SlotId) -> Tuple[Fact, ...]:
        world = self._world
        return world.kernel.definitions(slot)

    def history(self, slot: SlotId) -> Tuple[Record, ...]:
        world = self._world
        return world.kernel.history(slot)

    def compile_pattern(
        self,
        namespace: str,
        subject: SourceTerm,
        predicate: Union[str, Predicate],
        object_: SourceTerm,
    ) -> Pattern:
        world = self._world
        return world.kernel.compile_pattern(namespace, subject, predicate, object_)

    def render_binding(self, binding: Mapping[str, BoundValue]) -> Mapping[str, str]:
        world = self._world
        return world.kernel.render_binding(binding)

    def explain(self, result: QueryResult) -> Mapping[str, object]:
        world = self._world
        return world.kernel.explain(result)

    @staticmethod
    def display_slot(entity: EntityId) -> SlotId:
        return ForthDB.display_slot(entity)

    @staticmethod
    def symbol_slot(namespace: str, symbol: Symbol) -> SlotId:
        return ForthDB.symbol_slot(namespace, symbol)

    def define_display_name(self, tx: WorldTransaction, entity: EntityId, name: str) -> None:
        tx.define(self.display_slot(entity), Fact(entity, ForthDB.DISPLAY_NAME, Literal(name)))

    def bind_symbol(self, tx: WorldTransaction, namespace: str, symbol: Symbol, entity: EntityId) -> None:
        tx.define(
            self.symbol_slot(namespace, symbol),
            Fact(Literal(f"{namespace}:{symbol.value}"), ForthDB.RESOLVES_TO, entity),
        )

    def _commit(self, tx: WorldTransaction, *, fail_after_fsync: bool = False) -> CommitReceipt:
        # Candidate construction occurs before entering the small publication gate.
        candidate, records = tx._candidate()

        with self._commit_lock:
            current = self._world
            if current.version != tx._base.version or current.digest != tx._base.digest:
                raise TransactionConflict(
                    f"Transaction began at world {tx._base.version}, current world is {current.version}"
                )

            version = current.version + 1
            world_digest = _world_digest(candidate)
            operations = [
                _encode_operation(operation, version=version, ordinal=index)
                for index, operation in enumerate(tx._operations)
            ]
            frame = {
                "format": FORMAT_VERSION,
                "version": version,
                "parent_digest": current.digest,
                "operations": operations,
                "next_entity": candidate._next_entity,
                "world_digest": world_digest,
            }
            payload = _canonical_bytes(frame)
            framed = _frame_bytes(payload)

            with open(self.path, "ab", buffering=0) as log_file:
                log_file.write(framed)
                os.fsync(log_file.fileno())

            if fail_after_fsync:
                # Models process death after durable commit but before in-memory publication.
                raise RuntimeError("Injected crash after fsync and before publication")

            # One facade-level publication point. Existing snapshots retain the old world.
            self._world = CommittedWorld(version, world_digest, candidate)

            return CommitReceipt(
                version=version,
                parent_digest=current.digest,
                world_digest=world_digest,
                operation_count=len(tx._operations),
                records=records,
            )

    def _recover(self) -> CommittedWorld:
        kernel = ForthDB()
        version = 0
        digest = GENESIS_DIGEST

        if not self.path.exists():
            self.path.touch()
            return CommittedWorld(version, digest, kernel)

        with open(self.path, "rb") as log_file:
            while True:
                frame_start = log_file.tell()
                header = log_file.read(HEADER.size)
                if not header:
                    break
                if len(header) < HEADER.size:
                    # Incomplete tail cannot name a committed world.
                    break

                magic, payload_length = HEADER.unpack(header)
                if magic != MAGIC:
                    raise LogCorruption(f"Invalid frame magic at byte {frame_start}")

                payload = log_file.read(payload_length)
                if len(payload) < payload_length:
                    break

                trailer = log_file.read(TRAILER.size)
                if len(trailer) < TRAILER.size:
                    break

                checksum, repeated_length = TRAILER.unpack(trailer)
                if repeated_length != payload_length:
                    raise LogCorruption(f"Frame length mismatch at byte {frame_start}")
                if hashlib.sha256(payload).digest() != checksum:
                    raise LogCorruption(f"Frame checksum mismatch at byte {frame_start}")

                try:
                    frame = json.loads(payload.decode("utf-8"))
                except (UnicodeDecodeError, json.JSONDecodeError) as exc:
                    raise LogCorruption(f"Invalid frame payload at byte {frame_start}") from exc

                if not isinstance(frame, Mapping):
                    raise LogCorruption(f"Frame is not an object at byte {frame_start}")
                if frame.get("format") != FORMAT_VERSION:
                    raise LogCorruption(f"Unsupported frame format at byte {frame_start}")
                expected_version = version + 1
                if frame.get("version") != expected_version:
                    raise LogCorruption(
                        f"Expected commit version {expected_version}, got {frame.get('version')!r}"
                    )
                if frame.get("parent_digest") != digest:
                    raise LogCorruption(f"Parent digest mismatch at commit {expected_version}")

                raw_operations = frame.get("operations")
                if not isinstance(raw_operations, list):
                    raise LogCorruption(f"Invalid operations at commit {expected_version}")
                operations = []
                for raw_operation in raw_operations:
                    if not isinstance(raw_operation, Mapping):
                        raise LogCorruption(f"Invalid operation at commit {expected_version}")
                    operations.append(_decode_operation(raw_operation))

                candidate = _clone_kernel(kernel)
                _apply_operations(candidate, operations)
                next_entity = frame.get("next_entity")
                if not isinstance(next_entity, int) or next_entity < candidate._next_entity:
                    raise LogCorruption(f"Invalid entity allocator at commit {expected_version}")
                candidate._next_entity = next_entity
                candidate.validate()

                computed_digest = _world_digest(candidate)
                declared_digest = frame.get("world_digest")
                if not isinstance(declared_digest, str) or declared_digest != computed_digest:
                    raise LogCorruption(f"World digest mismatch at commit {expected_version}")

                kernel = candidate
                version = expected_version
                digest = computed_digest

        return CommittedWorld(version, digest, kernel)
