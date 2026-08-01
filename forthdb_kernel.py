
from __future__ import annotations

from collections import defaultdict
from dataclasses import dataclass, field
from typing import Dict, Iterable, Iterator, List, Mapping, Optional, Sequence, Tuple, Union


# ============================================================
# Typed public values
# ============================================================

@dataclass(frozen=True, order=True)
class EntityId:
    value: int

    def __str__(self) -> str:
        return f"Entity_{self.value}"


@dataclass(frozen=True, order=True)
class SlotId:
    value: str

    def __str__(self) -> str:
        return self.value


@dataclass(frozen=True, order=True)
class RecordId:
    value: int

    def __str__(self) -> str:
        return f"Record_{self.value}"


@dataclass(frozen=True, order=True)
class Symbol:
    value: str


@dataclass(frozen=True, order=True)
class Literal:
    value: str

    def __str__(self) -> str:
        return self.value


@dataclass(frozen=True, order=True)
class Predicate:
    name: str

    def __str__(self) -> str:
        return self.name


@dataclass(frozen=True, order=True)
class Variable:
    name: str

    def __post_init__(self) -> None:
        if not self.name or self.name.startswith("?"):
            raise ValueError("Variable names must be nonempty and omit the leading '?'")

    def __str__(self) -> str:
        return f"?{self.name}"


Atom = Union[EntityId, Literal]
BoundValue = Union[EntityId, Literal, Predicate]
SubjectTerm = Union[Atom, Variable]
PredicateTerm = Union[Predicate, Variable]
ObjectTerm = Union[Atom, Variable]
SourceTerm = Union[EntityId, Literal, Symbol, Variable]


@dataclass(frozen=True, order=True)
class Fact:
    subject: Atom
    predicate: Predicate
    object: Atom


@dataclass(frozen=True, order=True)
class Pattern:
    subject: SubjectTerm
    predicate: PredicateTerm
    object: ObjectTerm


@dataclass(frozen=True)
class Record:
    id: RecordId
    kind: str  # define | forget
    slot: SlotId
    fact: Optional[Fact]
    previous_head: Optional[RecordId]
    resulting_head: Optional[RecordId]


@dataclass(frozen=True)
class QueryRow:
    binding: Mapping[str, BoundValue]
    provenance: Tuple[SlotId, ...] = ()


@dataclass
class QueryMetrics:
    candidate_facts: int = 0
    bindings_emitted: int = 0
    candidates_by_pattern: Dict[str, int] = field(default_factory=lambda: defaultdict(int))
    emitted_by_pattern: Dict[str, int] = field(default_factory=lambda: defaultdict(int))


@dataclass(frozen=True)
class QueryResult:
    rows: Tuple[QueryRow, ...]
    chosen_first_path: Tuple[Pattern, ...]
    metrics: QueryMetrics

    def bindings(self) -> List[Dict[str, BoundValue]]:
        return [dict(row.binding) for row in self.rows]


# ============================================================
# Definition store
# ============================================================

class DefinitionStore:
    """Immutable record log plus one active definition head per SlotId."""

    def __init__(self) -> None:
        self.log: List[Record] = []
        self.head: Dict[SlotId, RecordId] = {}
        self.slot_history: Dict[SlotId, List[RecordId]] = defaultdict(list)

    def record(self, record_id: RecordId) -> Record:
        return self.log[record_id.value]

    def append_define(self, slot: SlotId, fact: Fact) -> Tuple[Record, Optional[RecordId]]:
        previous = self.head.get(slot)
        record_id = RecordId(len(self.log))
        record = Record(
            id=record_id,
            kind="define",
            slot=slot,
            fact=fact,
            previous_head=previous,
            resulting_head=record_id,
        )
        self.log.append(record)
        self.slot_history[slot].append(record_id)
        self.head[slot] = record_id
        return record, previous

    def append_forget(
        self,
        slot: SlotId,
    ) -> Tuple[Record, Optional[RecordId], Optional[RecordId]]:
        current = self.head.get(slot)

        if current is None:
            record_id = RecordId(len(self.log))
            record = Record(
                id=record_id,
                kind="forget",
                slot=slot,
                fact=None,
                previous_head=None,
                resulting_head=None,
            )
            self.log.append(record)
            self.slot_history[slot].append(record_id)
            return record, None, None

        revealed = self.record(current).previous_head
        record_id = RecordId(len(self.log))
        record = Record(
            id=record_id,
            kind="forget",
            slot=slot,
            fact=None,
            previous_head=current,
            resulting_head=revealed,
        )
        self.log.append(record)
        self.slot_history[slot].append(record_id)

        if revealed is None:
            del self.head[slot]
        else:
            self.head[slot] = revealed

        return record, current, revealed

    def resolve(self, slot: SlotId) -> Optional[Record]:
        record_id = self.head.get(slot)
        return None if record_id is None else self.record(record_id)

    def definitions(self, slot: SlotId) -> Iterator[Record]:
        record_id = self.head.get(slot)
        while record_id is not None:
            record = self.record(record_id)
            if record.kind != "define":
                raise AssertionError("Only definitions may be slot heads")
            yield record
            record_id = record.previous_head

    def history(self, slot: SlotId) -> Tuple[Record, ...]:
        return tuple(self.record(record_id) for record_id in self.slot_history.get(slot, ()))


# ============================================================
# Current indexed view
# ============================================================

class CurrentView:
    """Permutation-style indexes containing only active definition heads."""

    def __init__(self, store: DefinitionStore) -> None:
        self.store = store

        self.by_subject: Dict[Atom, set[RecordId]] = defaultdict(set)
        self.by_predicate: Dict[Predicate, set[RecordId]] = defaultdict(set)
        self.by_object: Dict[Atom, set[RecordId]] = defaultdict(set)
        self.by_subject_predicate: Dict[Tuple[Atom, Predicate], set[RecordId]] = defaultdict(set)
        self.by_subject_object: Dict[Tuple[Atom, Atom], set[RecordId]] = defaultdict(set)
        self.by_predicate_object: Dict[Tuple[Predicate, Atom], set[RecordId]] = defaultdict(set)
        self.by_exact: Dict[Fact, set[RecordId]] = defaultdict(set)

    def add(self, record_id: RecordId) -> None:
        record = self.store.record(record_id)
        if record.kind != "define" or record.fact is None:
            raise ValueError("Only definition records can enter the current view")

        fact = record.fact
        self.by_subject[fact.subject].add(record_id)
        self.by_predicate[fact.predicate].add(record_id)
        self.by_object[fact.object].add(record_id)
        self.by_subject_predicate[(fact.subject, fact.predicate)].add(record_id)
        self.by_subject_object[(fact.subject, fact.object)].add(record_id)
        self.by_predicate_object[(fact.predicate, fact.object)].add(record_id)
        self.by_exact[fact].add(record_id)

    def remove(self, record_id: RecordId) -> None:
        record = self.store.record(record_id)
        if record.fact is None:
            raise ValueError("Record has no fact")

        fact = record.fact
        entries = (
            (self.by_subject, fact.subject),
            (self.by_predicate, fact.predicate),
            (self.by_object, fact.object),
            (self.by_subject_predicate, (fact.subject, fact.predicate)),
            (self.by_subject_object, (fact.subject, fact.object)),
            (self.by_predicate_object, (fact.predicate, fact.object)),
            (self.by_exact, fact),
        )

        for index, key in entries:
            bucket = index[key]
            bucket.remove(record_id)
            if not bucket:
                del index[key]

    @staticmethod
    def _sorted_ids(values: Iterable[RecordId]) -> Tuple[RecordId, ...]:
        return tuple(sorted(values, key=lambda record_id: record_id.value))

    def all_active(self) -> Tuple[RecordId, ...]:
        return self._sorted_ids(self.store.head.values())

    def candidates(
        self,
        pattern: Pattern,
        binding: Mapping[str, BoundValue],
    ) -> Tuple[RecordId, ...]:
        subject = resolve_term(pattern.subject, binding)
        predicate = resolve_term(pattern.predicate, binding)
        object_ = resolve_term(pattern.object, binding)

        subject_known = not isinstance(subject, Variable)
        predicate_known = not isinstance(predicate, Variable)
        object_known = not isinstance(object_, Variable)

        if subject_known and predicate_known and object_known:
            assert isinstance(subject, (EntityId, Literal))
            assert isinstance(predicate, Predicate)
            assert isinstance(object_, (EntityId, Literal))
            return self._sorted_ids(self.by_exact.get(Fact(subject, predicate, object_), ()))

        if subject_known and predicate_known:
            assert isinstance(subject, (EntityId, Literal))
            assert isinstance(predicate, Predicate)
            return self._sorted_ids(self.by_subject_predicate.get((subject, predicate), ()))

        if subject_known and object_known:
            assert isinstance(subject, (EntityId, Literal))
            assert isinstance(object_, (EntityId, Literal))
            return self._sorted_ids(self.by_subject_object.get((subject, object_), ()))

        if predicate_known and object_known:
            assert isinstance(predicate, Predicate)
            assert isinstance(object_, (EntityId, Literal))
            return self._sorted_ids(self.by_predicate_object.get((predicate, object_), ()))

        if subject_known:
            assert isinstance(subject, (EntityId, Literal))
            return self._sorted_ids(self.by_subject.get(subject, ()))

        if predicate_known:
            assert isinstance(predicate, Predicate)
            return self._sorted_ids(self.by_predicate.get(predicate, ()))

        if object_known:
            assert isinstance(object_, (EntityId, Literal))
            return self._sorted_ids(self.by_object.get(object_, ()))

        return self.all_active()

    def validate(self) -> None:
        active = set(self.store.head.values())
        indexed: set[RecordId] = set()
        for bucket in self.by_subject_predicate.values():
            indexed.update(bucket)
        if active != indexed:
            raise AssertionError("Current-view indexes do not match active slot heads")


# ============================================================
# Query planning and execution
# ============================================================

def pattern_key(pattern: Pattern) -> str:
    return f"{pattern.subject} {pattern.predicate} {pattern.object}"


def bound_key(value: BoundValue) -> Tuple[str, str]:
    if isinstance(value, EntityId):
        return ("entity", str(value.value))
    if isinstance(value, Literal):
        return ("literal", value.value)
    if isinstance(value, Predicate):
        return ("predicate", value.name)
    raise TypeError(value)


def binding_key(binding: Mapping[str, BoundValue]) -> Tuple[Tuple[str, Tuple[str, str]], ...]:
    return tuple(sorted((name, bound_key(value)) for name, value in binding.items()))


def resolve_term(
    term: Union[SubjectTerm, PredicateTerm, ObjectTerm],
    binding: Mapping[str, BoundValue],
):
    if isinstance(term, Variable) and term.name in binding:
        return binding[term.name]
    return term


def unify(
    term: Union[SubjectTerm, PredicateTerm, ObjectTerm],
    value: BoundValue,
    binding: Mapping[str, BoundValue],
) -> Optional[Dict[str, BoundValue]]:
    if not isinstance(term, Variable):
        return dict(binding) if term == value else None

    existing = binding.get(term.name)
    if existing is not None:
        return dict(binding) if existing == value else None

    extended = dict(binding)
    extended[term.name] = value
    return extended


@dataclass(frozen=True)
class _Frame:
    binding: Mapping[str, BoundValue]
    provenance: Tuple[SlotId, ...]


class QueryExecutor:
    def __init__(self, store: DefinitionStore, view: CurrentView) -> None:
        self.store = store
        self.view = view

    def _match(
        self,
        pattern: Pattern,
        frame: _Frame,
        metrics: QueryMetrics,
    ) -> Iterator[_Frame]:
        candidate_ids = self.view.candidates(pattern, frame.binding)
        key = pattern_key(pattern)
        metrics.candidate_facts += len(candidate_ids)
        metrics.candidates_by_pattern[key] += len(candidate_ids)

        for record_id in candidate_ids:
            record = self.store.record(record_id)
            if record.fact is None:
                continue

            fact = record.fact
            binding: Dict[str, BoundValue] = dict(frame.binding)

            for term, value in (
                (pattern.subject, fact.subject),
                (pattern.predicate, fact.predicate),
                (pattern.object, fact.object),
            ):
                next_binding = unify(term, value, binding)
                if next_binding is None:
                    break
                binding = next_binding
            else:
                metrics.bindings_emitted += 1
                metrics.emitted_by_pattern[key] += 1
                yield _Frame(binding, frame.provenance + (record.slot,))

    def execute(
        self,
        patterns: Sequence[Pattern],
        *,
        optimize: bool = True,
        distinct: bool = True,
        include_provenance: bool = False,
        limit: Optional[int] = None,
    ) -> QueryResult:
        metrics = QueryMetrics()
        output: List[_Frame] = []
        first_path: List[Pattern] = []

        def walk(frame: _Frame, remaining: Tuple[Pattern, ...], depth: int) -> bool:
            if not remaining:
                output.append(frame)
                return limit is not None and len(output) >= limit

            if optimize:
                chosen = min(
                    enumerate(remaining),
                    key=lambda item: (
                        len(self.view.candidates(item[1], frame.binding)),
                        item[0],
                    ),
                )[1]
            else:
                chosen = remaining[0]

            if depth == len(first_path):
                first_path.append(chosen)

            rest = list(remaining)
            rest.remove(chosen)

            for next_frame in self._match(chosen, frame, metrics):
                if walk(next_frame, tuple(rest), depth + 1):
                    return True
            return False

        walk(_Frame({}, ()), tuple(patterns), 0)

        if distinct:
            deduped: Dict[Tuple[Tuple[str, Tuple[str, str]], ...], _Frame] = {}
            for frame in output:
                deduped.setdefault(binding_key(frame.binding), frame)
            output = list(deduped.values())

        output.sort(
            key=lambda frame: (
                binding_key(frame.binding),
                tuple(slot.value for slot in frame.provenance),
            )
        )

        rows = tuple(
            QueryRow(
                binding=dict(frame.binding),
                provenance=frame.provenance if include_provenance else (),
            )
            for frame in output
        )
        return QueryResult(rows, tuple(first_path), metrics)


# ============================================================
# Kernel facade
# ============================================================

class ForthDB:
    DISPLAY_NAME = Predicate("display_name")
    RESOLVES_TO = Predicate("resolves_to")

    def __init__(self) -> None:
        self.store = DefinitionStore()
        self.view = CurrentView(self.store)
        self.executor = QueryExecutor(self.store, self.view)
        self._next_entity = 1

    def entity(self) -> EntityId:
        entity = EntityId(self._next_entity)
        self._next_entity += 1
        return entity

    def define(self, slot: SlotId, fact: Fact) -> Record:
        old_head = self.store.head.get(slot)
        if old_head is not None:
            self.view.remove(old_head)

        record, _ = self.store.append_define(slot, fact)
        self.view.add(record.id)
        return record

    def forget(self, slot: SlotId) -> Record:
        current = self.store.head.get(slot)
        if current is not None:
            self.view.remove(current)

        record, _, revealed = self.store.append_forget(slot)
        if revealed is not None:
            self.view.add(revealed)
        return record

    def resolve(self, slot: SlotId) -> Optional[Fact]:
        record = self.store.resolve(slot)
        return None if record is None else record.fact

    def definitions(self, slot: SlotId) -> Tuple[Fact, ...]:
        return tuple(
            record.fact
            for record in self.store.definitions(slot)
            if record.fact is not None
        )

    def history(self, slot: SlotId) -> Tuple[Record, ...]:
        return self.store.history(slot)

    @staticmethod
    def display_slot(entity: EntityId) -> SlotId:
        return SlotId(f"display/{entity.value}")

    @staticmethod
    def symbol_slot(namespace: str, symbol: Symbol) -> SlotId:
        return SlotId(f"namespace/{namespace}/{symbol.value}")

    def define_display_name(self, entity: EntityId, name: str) -> Record:
        return self.define(
            self.display_slot(entity),
            Fact(entity, self.DISPLAY_NAME, Literal(name)),
        )

    def display_name(self, entity: EntityId) -> str:
        fact = self.resolve(self.display_slot(entity))
        if fact is None or not isinstance(fact.object, Literal):
            return str(entity)
        return fact.object.value

    def bind_symbol(self, namespace: str, symbol: Symbol, entity: EntityId) -> Record:
        return self.define(
            self.symbol_slot(namespace, symbol),
            Fact(Literal(f"{namespace}:{symbol.value}"), self.RESOLVES_TO, entity),
        )

    def resolve_symbol(self, namespace: str, symbol: Symbol) -> EntityId:
        fact = self.resolve(self.symbol_slot(namespace, symbol))
        if fact is None or not isinstance(fact.object, EntityId):
            raise KeyError(f"Unbound symbol: {namespace}:{symbol.value}")
        return fact.object

    def compile_term(self, namespace: str, term: SourceTerm) -> Union[Atom, Variable]:
        if isinstance(term, Symbol):
            return self.resolve_symbol(namespace, term)
        return term

    def compile_pattern(
        self,
        namespace: str,
        subject: SourceTerm,
        predicate: Union[str, Predicate],
        object_: SourceTerm,
    ) -> Pattern:
        compiled_subject = self.compile_term(namespace, subject)
        compiled_object = self.compile_term(namespace, object_)
        compiled_predicate = Predicate(predicate) if isinstance(predicate, str) else predicate
        return Pattern(compiled_subject, compiled_predicate, compiled_object)

    def query(
        self,
        patterns: Sequence[Pattern],
        *,
        optimize: bool = True,
        distinct: bool = True,
        include_provenance: bool = False,
        limit: Optional[int] = None,
    ) -> QueryResult:
        return self.executor.execute(
            patterns,
            optimize=optimize,
            distinct=distinct,
            include_provenance=include_provenance,
            limit=limit,
        )

    def render_value(self, value: BoundValue) -> str:
        if isinstance(value, EntityId):
            return self.display_name(value)
        if isinstance(value, Literal):
            return value.value
        if isinstance(value, Predicate):
            return value.name
        raise TypeError(value)

    def render_binding(self, binding: Mapping[str, BoundValue]) -> Dict[str, str]:
        return {
            name: self.render_value(value)
            for name, value in sorted(binding.items())
        }

    def explain(self, result: QueryResult) -> Dict[str, object]:
        return {
            "chosen_first_path": [pattern_key(pattern) for pattern in result.chosen_first_path],
            "candidate_facts": result.metrics.candidate_facts,
            "bindings_emitted": result.metrics.bindings_emitted,
            "candidates_by_pattern": dict(result.metrics.candidates_by_pattern),
            "emitted_by_pattern": dict(result.metrics.emitted_by_pattern),
            "rows": len(result.rows),
        }

    def validate(self) -> None:
        self.view.validate()
