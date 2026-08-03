//! Experimental in-memory semantic VM for the Phase 1 arena/stack benchmark.
//!
//! This deliberately does not change the durable admission format. It isolates
//! the cost of materializing rejectable intents without cloning `ForthDb`.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

const NONE: u32 = u32::MAX;

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Cell(pub u64);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SlotToken(pub u32);

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Opcode {
    ExpectObject,
    Allocate,
    AllocateDiscard,
    LoadLocal,
    StoreLocal,
    PushCell,
    Define,
    Forget,
    Reject,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Instruction {
    opcode: Opcode,
    argument: u32,
    immediate: u64,
}

impl Instruction {
    pub const fn expect_object(slot: SlotToken, expected: Cell) -> Self {
        Self {
            opcode: Opcode::ExpectObject,
            argument: slot.0,
            immediate: expected.0,
        }
    }

    pub const fn allocate() -> Self {
        Self {
            opcode: Opcode::Allocate,
            argument: 0,
            immediate: 0,
        }
    }

    pub(crate) const fn allocate_discard() -> Self {
        Self {
            opcode: Opcode::AllocateDiscard,
            argument: 0,
            immediate: 0,
        }
    }

    pub const fn load_local(local: u32) -> Self {
        Self {
            opcode: Opcode::LoadLocal,
            argument: local,
            immediate: 0,
        }
    }

    pub const fn store_local(local: u32) -> Self {
        Self {
            opcode: Opcode::StoreLocal,
            argument: local,
            immediate: 0,
        }
    }

    pub const fn push(cell: Cell) -> Self {
        Self {
            opcode: Opcode::PushCell,
            argument: 0,
            immediate: cell.0,
        }
    }

    pub const fn define(slot: SlotToken) -> Self {
        Self {
            opcode: Opcode::Define,
            argument: slot.0,
            immediate: 0,
        }
    }

    pub const fn forget(slot: SlotToken) -> Self {
        Self {
            opcode: Opcode::Forget,
            argument: slot.0,
            immediate: 0,
        }
    }

    pub const fn reject() -> Self {
        Self {
            opcode: Opcode::Reject,
            argument: 0,
            immediate: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntentProgram {
    local_count: u32,
    instructions: Vec<Instruction>,
}

impl IntentProgram {
    pub fn new(local_count: u32, instructions: Vec<Instruction>) -> Self {
        Self {
            local_count,
            instructions,
        }
    }

    pub fn instructions(&self) -> &[Instruction] {
        &self.instructions
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordKind {
    Define,
    Forget,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FactRecord {
    kind: RecordKind,
    slot: SlotToken,
    subject: Cell,
    predicate: Cell,
    object: Cell,
    previous_visible: u32,
    resulting_visible: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlotDelta {
    slot: SlotToken,
    previous_visible: u32,
    resulting_visible: u32,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexPermutation {
    Spo,
    Pos,
    Osp,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndexDelta {
    permutation: IndexPermutation,
    added: bool,
    first: Cell,
    second: Cell,
    third: Cell,
    record: u32,
    slot: SlotToken,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorldRoot {
    parent: u32,
    version: u64,
    record_frontier: u32,
    slot_delta_frontier: u32,
    index_delta_frontier: u32,
    allocator_head: u64,
    world_id: u64,
}

impl WorldRoot {
    pub const fn version(self) -> u64 {
        self.version
    }

    pub const fn allocator_head(self) -> u64 {
        self.allocator_head
    }

    pub const fn world_id(self) -> u64 {
        self.world_id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryPattern {
    Subject(Cell),
    Predicate(Cell),
    Object(Cell),
    SubjectPredicate(Cell, Cell),
    SubjectObject(Cell, Cell),
    PredicateObject(Cell, Cell),
    Exact(Cell, Cell, Cell),
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CompactIndexEntry {
    first: Cell,
    second: Cell,
    third: Cell,
    record: u32,
    slot: SlotToken,
}

/// A compact immutable query base. Each permutation is a contiguous sorted
/// array suitable for binary prefix lookup.
pub struct CompactBase {
    root: WorldRoot,
    spo: Box<[CompactIndexEntry]>,
    pos: Box<[CompactIndexEntry]>,
    osp: Box<[CompactIndexEntry]>,
}

impl CompactBase {
    pub fn active_fact_count(&self) -> usize {
        self.spo.len()
    }

    pub fn world_id(&self) -> u64 {
        self.root.world_id
    }
}

/// A reader-owned world: an `Arc`-shared compact base plus the short immutable
/// delta tail needed to reach a later root.
pub struct LayeredSnapshot {
    base: Arc<CompactBase>,
    root: WorldRoot,
    tail: Arc<[IndexDelta]>,
}

impl LayeredSnapshot {
    pub fn world_id(&self) -> u64 {
        self.root.world_id
    }

    pub fn tail_delta_count(&self) -> usize {
        self.tail.len()
    }

    pub fn query_count(&self, pattern: QueryPattern) -> usize {
        let (entries, key, key_len) = self.base_entries(pattern);
        let range = compact_prefix_range(entries, key, key_len);
        let mut count = range.end as i64 - range.start as i64;
        for delta in self.tail.iter().copied() {
            if matches_index(delta, pattern) {
                count += if delta.added { 1 } else { -1 };
            }
        }
        debug_assert!(count >= 0);
        count as usize
    }

    pub fn query_slots(&self, pattern: QueryPattern) -> Vec<SlotToken> {
        let (entries, key, key_len) = self.base_entries(pattern);
        let range = compact_prefix_range(entries, key, key_len);
        let mut active = entries[range]
            .iter()
            .map(|entry| (entry.record, entry.slot))
            .collect::<Vec<_>>();
        for delta in self.tail.iter().copied() {
            if !matches_index(delta, pattern) {
                continue;
            }
            if delta.added {
                active.push((delta.record, delta.slot));
            } else if let Some(position) = active
                .iter()
                .position(|(record, _)| *record == delta.record)
            {
                active.swap_remove(position);
            }
        }
        let mut slots = active.into_iter().map(|(_, slot)| slot).collect::<Vec<_>>();
        slots.sort_unstable();
        slots
    }

    fn base_entries(&self, pattern: QueryPattern) -> (&[CompactIndexEntry], [Cell; 3], usize) {
        let (permutation, key, key_len) = query_key(pattern);
        let entries = match permutation {
            IndexPermutation::Spo => self.base.spo.as_ref(),
            IndexPermutation::Pos => self.base.pos.as_ref(),
            IndexPermutation::Osp => self.base.osp.as_ref(),
        };
        (entries, key, key_len)
    }
}

#[derive(Clone, Debug)]
struct PodArena<T: Copy> {
    data: Vec<T>,
    frontier: usize,
}

impl<T: Copy> PodArena<T> {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            data: Vec::with_capacity(capacity),
            frontier: 0,
        }
    }

    #[inline]
    fn frontier(&self) -> usize {
        self.frontier
    }

    #[inline]
    fn restore(&mut self, frontier: usize) {
        debug_assert!(frontier <= self.frontier);
        self.frontier = frontier;
    }

    #[inline]
    fn push(&mut self, value: T) -> u32 {
        let index = self.frontier;
        assert!(
            index < NONE as usize,
            "Phase 1 arena exhausted u32 references"
        );
        if index == self.data.len() {
            self.data.push(value);
        } else {
            self.data[index] = value;
        }
        self.frontier += 1;
        index as u32
    }

    #[inline]
    fn get(&self, index: u32) -> &T {
        let index = index as usize;
        assert!(
            index < self.frontier,
            "record reference beyond active frontier"
        );
        &self.data[index]
    }

    fn active(&self) -> &[T] {
        &self.data[..self.frontier]
    }
}

#[derive(Clone, Copy, Debug)]
struct TrialCheckpoint {
    stack_pointer: usize,
    frame_base: usize,
    record_frontier: usize,
    delta_frontier: usize,
    index_delta_frontier: usize,
    next_entity: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecError {
    ExplicitRejection,
    MissingExpectedValue,
    ExpectedValueMismatch,
    StackUnderflow,
    StackOverflow,
    InvalidLocal,
    InvalidSlot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionOutcome {
    Accepted,
    Rejected(ExecError),
}

/// A single-threaded epoch workspace. Trial writes remain private until an
/// intent is accepted, so rejection restores POD frontiers without undoing a
/// shared hash map or dropping heap-owned values.
pub struct Workspace {
    stack: Vec<Cell>,
    stack_pointer: usize,
    frame_base: usize,
    records: PodArena<FactRecord>,
    deltas: PodArena<SlotDelta>,
    index_deltas: PodArena<IndexDelta>,
    roots: PodArena<WorldRoot>,
    accepted_heads: Vec<u32>,
    next_entity: u64,
    last_root: u32,
    version: u64,
    semantic_hash: u64,
    track_indexes: bool,
}

impl Workspace {
    pub fn with_capacity(
        slot_count: usize,
        stack_capacity: usize,
        record_capacity: usize,
        delta_capacity: usize,
    ) -> Self {
        Self::new(
            slot_count,
            stack_capacity,
            record_capacity,
            delta_capacity,
            false,
        )
    }

    pub fn with_indexes(
        slot_count: usize,
        stack_capacity: usize,
        record_capacity: usize,
        delta_capacity: usize,
    ) -> Self {
        Self::new(
            slot_count,
            stack_capacity,
            record_capacity,
            delta_capacity,
            true,
        )
    }

    pub(crate) fn with_indexes_from(
        next_entity: u64,
        slot_count: usize,
        stack_capacity: usize,
        record_capacity: usize,
        delta_capacity: usize,
    ) -> Self {
        let mut workspace = Self::new(
            slot_count,
            stack_capacity,
            record_capacity,
            delta_capacity,
            true,
        );
        workspace.next_entity = next_entity;
        workspace
    }

    fn new(
        slot_count: usize,
        stack_capacity: usize,
        record_capacity: usize,
        delta_capacity: usize,
        track_indexes: bool,
    ) -> Self {
        Self {
            stack: vec![Cell::default(); stack_capacity],
            stack_pointer: 0,
            frame_base: 0,
            records: PodArena::with_capacity(record_capacity),
            deltas: PodArena::with_capacity(delta_capacity),
            index_deltas: PodArena::with_capacity(if track_indexes {
                record_capacity.saturating_mul(6)
            } else {
                0
            }),
            roots: PodArena::with_capacity(1_024),
            accepted_heads: vec![NONE; slot_count],
            next_entity: 1,
            last_root: NONE,
            version: 0,
            semantic_hash: 0xcbf2_9ce4_8422_2325,
            track_indexes,
        }
    }

    pub fn execute(&mut self, program: &IntentProgram) -> ExecutionOutcome {
        let checkpoint = self.checkpoint();
        self.frame_base = self.stack_pointer;
        if self.reserve(program.local_count as usize).is_err() {
            self.rollback(checkpoint);
            return ExecutionOutcome::Rejected(ExecError::StackOverflow);
        }

        for instruction in program.instructions() {
            if let Err(error) = self.execute_instruction(*instruction, checkpoint) {
                self.rollback(checkpoint);
                return ExecutionOutcome::Rejected(error);
            }
        }

        self.accept(checkpoint);
        ExecutionOutcome::Accepted
    }

    pub fn resolve_object(&self, slot: SlotToken) -> Option<Cell> {
        let head = self.accepted_head(slot).ok()?;
        (head != NONE).then(|| self.records.get(head).object)
    }

    pub(crate) fn resolve_fact_cells(&self, slot: SlotToken) -> Option<(Cell, Cell, Cell)> {
        let head = self.accepted_head(slot).ok()?;
        (head != NONE).then(|| {
            let record = self.records.get(head);
            (record.subject, record.predicate, record.object)
        })
    }

    pub(crate) fn ensure_slot_count(&mut self, slot_count: usize) {
        if self.accepted_heads.len() < slot_count {
            self.accepted_heads.resize(slot_count, NONE);
        }
    }

    pub fn next_entity(&self) -> u64 {
        self.next_entity
    }

    pub fn record_count(&self) -> usize {
        self.records.frontier()
    }

    pub fn delta_count(&self) -> usize {
        self.deltas.frontier()
    }

    pub fn index_delta_count(&self) -> usize {
        self.index_deltas.frontier()
    }

    pub fn active_slot_count(&self) -> usize {
        self.accepted_heads
            .iter()
            .filter(|head| **head != NONE)
            .count()
    }

    /// Publish a POD world descriptor over the accepted arena frontiers.
    /// The backing arenas remain append-only, so older roots continue to see
    /// exactly their prefix without copying records or allocating an `Arc`.
    pub fn publish_epoch(&mut self) -> Option<WorldRoot> {
        let previous_record_frontier = if self.last_root == NONE {
            0
        } else {
            self.roots.get(self.last_root).record_frontier as usize
        };
        if self.records.frontier() == previous_record_frontier {
            return None;
        }
        self.version += 1;
        let root = WorldRoot {
            parent: self.last_root,
            version: self.version,
            record_frontier: self.records.frontier() as u32,
            slot_delta_frontier: self.deltas.frontier() as u32,
            index_delta_frontier: self.index_deltas.frontier() as u32,
            allocator_head: self.next_entity,
            world_id: self.semantic_hash,
        };
        self.last_root = self.roots.push(root);
        Some(root)
    }

    pub fn resolve_object_at(&self, root: WorldRoot, slot: SlotToken) -> Option<Cell> {
        self.deltas.active()[..root.slot_delta_frontier as usize]
            .iter()
            .rev()
            .find(|delta| delta.slot == slot)
            .and_then(|delta| {
                (delta.resulting_visible != NONE)
                    .then(|| self.records.get(delta.resulting_visible).object)
            })
    }

    /// Query an immutable root through the three permutation-delta streams.
    /// This Phase 2 reader intentionally favors a simple differential oracle;
    /// compaction into searchable base segments is a later concern.
    pub fn query_slots(&self, root: WorldRoot, pattern: QueryPattern) -> Vec<SlotToken> {
        let mut active_records = Vec::<u32>::new();
        for delta in &self.index_deltas.active()[..root.index_delta_frontier as usize] {
            if !matches_index(*delta, pattern) {
                continue;
            }
            if delta.added {
                active_records.push(delta.record);
            } else if let Some(position) = active_records
                .iter()
                .position(|record| *record == delta.record)
            {
                active_records.swap_remove(position);
            }
        }
        let mut slots = active_records
            .into_iter()
            .map(|record| self.records.get(record).slot)
            .collect::<Vec<_>>();
        slots.sort_unstable_by_key(|slot| slot.0);
        slots
    }

    /// Consolidate one root into three sorted immutable permutation arrays.
    /// Compaction allocates and is intentionally outside the materialization
    /// and publication hot path measured by Phase 2.
    pub fn compact_base(&self, root: WorldRoot) -> CompactBase {
        let mut active = BTreeMap::<u32, IndexDelta>::new();
        for delta in &self.index_deltas.active()[..root.index_delta_frontier as usize] {
            if delta.permutation != IndexPermutation::Spo {
                continue;
            }
            if delta.added {
                active.insert(delta.record, *delta);
            } else {
                active.remove(&delta.record);
            }
        }

        let mut spo = Vec::with_capacity(active.len());
        let mut pos = Vec::with_capacity(active.len());
        let mut osp = Vec::with_capacity(active.len());
        for delta in active.values().copied() {
            spo.push(CompactIndexEntry {
                first: delta.first,
                second: delta.second,
                third: delta.third,
                record: delta.record,
                slot: delta.slot,
            });
            pos.push(CompactIndexEntry {
                first: delta.second,
                second: delta.third,
                third: delta.first,
                record: delta.record,
                slot: delta.slot,
            });
            osp.push(CompactIndexEntry {
                first: delta.third,
                second: delta.first,
                third: delta.second,
                record: delta.record,
                slot: delta.slot,
            });
        }
        spo.sort_unstable();
        pos.sort_unstable();
        osp.sort_unstable();
        CompactBase {
            root,
            spo: spo.into_boxed_slice(),
            pos: pos.into_boxed_slice(),
            osp: osp.into_boxed_slice(),
        }
    }

    pub fn layered_snapshot(&self, base: Arc<CompactBase>, root: WorldRoot) -> LayeredSnapshot {
        assert!(base.root.index_delta_frontier <= root.index_delta_frontier);
        let tail = self.index_deltas.active()
            [base.root.index_delta_frontier as usize..root.index_delta_frontier as usize]
            .to_vec();
        LayeredSnapshot {
            base,
            root,
            tail: Arc::from(tail),
        }
    }

    fn checkpoint(&self) -> TrialCheckpoint {
        TrialCheckpoint {
            stack_pointer: self.stack_pointer,
            frame_base: self.frame_base,
            record_frontier: self.records.frontier(),
            delta_frontier: self.deltas.frontier(),
            index_delta_frontier: self.index_deltas.frontier(),
            next_entity: self.next_entity,
        }
    }

    #[inline]
    fn rollback(&mut self, checkpoint: TrialCheckpoint) {
        self.stack_pointer = checkpoint.stack_pointer;
        self.frame_base = checkpoint.frame_base;
        self.records.restore(checkpoint.record_frontier);
        self.deltas.restore(checkpoint.delta_frontier);
        self.index_deltas.restore(checkpoint.index_delta_frontier);
        self.next_entity = checkpoint.next_entity;
    }

    fn accept(&mut self, checkpoint: TrialCheckpoint) {
        for delta in &self.deltas.active()[checkpoint.delta_frontier..] {
            self.accepted_heads[delta.slot.0 as usize] = delta.resulting_visible;
        }
        for record in &self.records.active()[checkpoint.record_frontier..] {
            self.semantic_hash = hash_record(self.semantic_hash, *record);
        }
        self.stack_pointer = checkpoint.stack_pointer;
        self.frame_base = checkpoint.frame_base;
    }

    fn execute_instruction(
        &mut self,
        instruction: Instruction,
        checkpoint: TrialCheckpoint,
    ) -> Result<(), ExecError> {
        match instruction.opcode {
            Opcode::ExpectObject => {
                let slot = SlotToken(instruction.argument);
                let expected = Cell(instruction.immediate);
                match self.resolve_trial_object(slot, checkpoint.delta_frontier)? {
                    Some(actual) if actual == expected => Ok(()),
                    Some(_) => Err(ExecError::ExpectedValueMismatch),
                    None => Err(ExecError::MissingExpectedValue),
                }
            }
            Opcode::Allocate => {
                let entity = Cell(self.next_entity);
                self.next_entity = self.next_entity.saturating_add(1);
                self.push(entity)
            }
            Opcode::AllocateDiscard => {
                self.next_entity = self.next_entity.saturating_add(1);
                Ok(())
            }
            Opcode::LoadLocal => {
                let value = self.local(instruction.argument as usize)?;
                self.push(value)
            }
            Opcode::StoreLocal => {
                let value = self.pop()?;
                let local = self.local_mut(instruction.argument as usize)?;
                *local = value;
                Ok(())
            }
            Opcode::PushCell => self.push(Cell(instruction.immediate)),
            Opcode::Define => {
                let object = self.pop()?;
                let predicate = self.pop()?;
                let subject = self.pop()?;
                self.define(
                    SlotToken(instruction.argument),
                    subject,
                    predicate,
                    object,
                    checkpoint.delta_frontier,
                )
            }
            Opcode::Forget => {
                self.forget(SlotToken(instruction.argument), checkpoint.delta_frontier)
            }
            Opcode::Reject => Err(ExecError::ExplicitRejection),
        }
    }

    fn define(
        &mut self,
        slot: SlotToken,
        subject: Cell,
        predicate: Cell,
        object: Cell,
        trial_delta_frontier: usize,
    ) -> Result<(), ExecError> {
        let previous = self.resolve_trial_head(slot, trial_delta_frontier)?;
        let record_index = self.records.frontier() as u32;
        let record = FactRecord {
            kind: RecordKind::Define,
            slot,
            subject,
            predicate,
            object,
            previous_visible: previous,
            resulting_visible: record_index,
        };
        let actual = self.records.push(record);
        debug_assert_eq!(actual, record_index);
        self.deltas.push(SlotDelta {
            slot,
            previous_visible: previous,
            resulting_visible: record_index,
        });
        self.append_index_transition(previous, record_index);
        Ok(())
    }

    fn forget(&mut self, slot: SlotToken, trial_delta_frontier: usize) -> Result<(), ExecError> {
        let previous = self.resolve_trial_head(slot, trial_delta_frontier)?;
        let revealed = if previous == NONE {
            NONE
        } else {
            self.records.get(previous).previous_visible
        };
        let record = FactRecord {
            kind: RecordKind::Forget,
            slot,
            subject: Cell(0),
            predicate: Cell(0),
            object: Cell(0),
            previous_visible: previous,
            resulting_visible: revealed,
        };
        self.records.push(record);
        self.deltas.push(SlotDelta {
            slot,
            previous_visible: previous,
            resulting_visible: revealed,
        });
        self.append_index_transition(previous, revealed);
        Ok(())
    }

    fn append_index_transition(&mut self, previous: u32, resulting: u32) {
        if !self.track_indexes {
            return;
        }
        if previous != NONE {
            let fact = *self.records.get(previous);
            self.append_fact_indexes(fact, previous, false);
        }
        if resulting != NONE {
            let fact = *self.records.get(resulting);
            self.append_fact_indexes(fact, resulting, true);
        }
    }

    fn append_fact_indexes(&mut self, fact: FactRecord, record: u32, added: bool) {
        self.index_deltas.push(IndexDelta {
            permutation: IndexPermutation::Spo,
            added,
            first: fact.subject,
            second: fact.predicate,
            third: fact.object,
            record,
            slot: fact.slot,
        });
        self.index_deltas.push(IndexDelta {
            permutation: IndexPermutation::Pos,
            added,
            first: fact.predicate,
            second: fact.object,
            third: fact.subject,
            record,
            slot: fact.slot,
        });
        self.index_deltas.push(IndexDelta {
            permutation: IndexPermutation::Osp,
            added,
            first: fact.object,
            second: fact.subject,
            third: fact.predicate,
            record,
            slot: fact.slot,
        });
    }

    fn resolve_trial_object(
        &self,
        slot: SlotToken,
        trial_delta_frontier: usize,
    ) -> Result<Option<Cell>, ExecError> {
        let head = self.resolve_trial_head(slot, trial_delta_frontier)?;
        Ok((head != NONE).then(|| self.records.get(head).object))
    }

    fn resolve_trial_head(
        &self,
        slot: SlotToken,
        trial_delta_frontier: usize,
    ) -> Result<u32, ExecError> {
        self.accepted_head(slot)?;
        for delta in self.deltas.active()[trial_delta_frontier..].iter().rev() {
            if delta.slot == slot {
                return Ok(delta.resulting_visible);
            }
        }
        self.accepted_head(slot)
    }

    fn accepted_head(&self, slot: SlotToken) -> Result<u32, ExecError> {
        self.accepted_heads
            .get(slot.0 as usize)
            .copied()
            .ok_or(ExecError::InvalidSlot)
    }

    fn reserve(&mut self, cells: usize) -> Result<(), ExecError> {
        if self.stack_pointer.saturating_add(cells) > self.stack.len() {
            return Err(ExecError::StackOverflow);
        }
        self.stack_pointer += cells;
        Ok(())
    }

    fn push(&mut self, value: Cell) -> Result<(), ExecError> {
        if self.stack_pointer == self.stack.len() {
            return Err(ExecError::StackOverflow);
        }
        self.stack[self.stack_pointer] = value;
        self.stack_pointer += 1;
        Ok(())
    }

    fn pop(&mut self) -> Result<Cell, ExecError> {
        if self.stack_pointer <= self.frame_base {
            return Err(ExecError::StackUnderflow);
        }
        self.stack_pointer -= 1;
        Ok(self.stack[self.stack_pointer])
    }

    fn local(&self, local: usize) -> Result<Cell, ExecError> {
        let index = self.frame_base.saturating_add(local);
        if index >= self.stack_pointer {
            return Err(ExecError::InvalidLocal);
        }
        Ok(self.stack[index])
    }

    fn local_mut(&mut self, local: usize) -> Result<&mut Cell, ExecError> {
        let index = self.frame_base.saturating_add(local);
        if index >= self.stack_pointer {
            return Err(ExecError::InvalidLocal);
        }
        Ok(&mut self.stack[index])
    }
}

fn matches_index(delta: IndexDelta, pattern: QueryPattern) -> bool {
    match pattern {
        QueryPattern::Subject(subject) => {
            delta.permutation == IndexPermutation::Spo && delta.first == subject
        }
        QueryPattern::Predicate(predicate) => {
            delta.permutation == IndexPermutation::Pos && delta.first == predicate
        }
        QueryPattern::Object(object) => {
            delta.permutation == IndexPermutation::Osp && delta.first == object
        }
        QueryPattern::SubjectPredicate(subject, predicate) => {
            delta.permutation == IndexPermutation::Spo
                && delta.first == subject
                && delta.second == predicate
        }
        QueryPattern::SubjectObject(subject, object) => {
            delta.permutation == IndexPermutation::Osp
                && delta.first == object
                && delta.second == subject
        }
        QueryPattern::PredicateObject(predicate, object) => {
            delta.permutation == IndexPermutation::Pos
                && delta.first == predicate
                && delta.second == object
        }
        QueryPattern::Exact(subject, predicate, object) => {
            delta.permutation == IndexPermutation::Spo
                && delta.first == subject
                && delta.second == predicate
                && delta.third == object
        }
    }
}

fn query_key(pattern: QueryPattern) -> (IndexPermutation, [Cell; 3], usize) {
    match pattern {
        QueryPattern::Subject(subject) => (IndexPermutation::Spo, [subject, Cell(0), Cell(0)], 1),
        QueryPattern::Predicate(predicate) => {
            (IndexPermutation::Pos, [predicate, Cell(0), Cell(0)], 1)
        }
        QueryPattern::Object(object) => (IndexPermutation::Osp, [object, Cell(0), Cell(0)], 1),
        QueryPattern::SubjectPredicate(subject, predicate) => {
            (IndexPermutation::Spo, [subject, predicate, Cell(0)], 2)
        }
        QueryPattern::SubjectObject(subject, object) => {
            (IndexPermutation::Osp, [object, subject, Cell(0)], 2)
        }
        QueryPattern::PredicateObject(predicate, object) => {
            (IndexPermutation::Pos, [predicate, object, Cell(0)], 2)
        }
        QueryPattern::Exact(subject, predicate, object) => {
            (IndexPermutation::Spo, [subject, predicate, object], 3)
        }
    }
}

fn compare_prefix(entry: &CompactIndexEntry, key: [Cell; 3], key_len: usize) -> Ordering {
    let entry_key = [entry.first, entry.second, entry.third];
    entry_key[..key_len].cmp(&key[..key_len])
}

fn compact_prefix_range(
    entries: &[CompactIndexEntry],
    key: [Cell; 3],
    key_len: usize,
) -> std::ops::Range<usize> {
    let start = entries.partition_point(|entry| compare_prefix(entry, key, key_len).is_lt());
    let end = entries.partition_point(|entry| !compare_prefix(entry, key, key_len).is_gt());
    start..end
}

fn hash_record(mut hash: u64, record: FactRecord) -> u64 {
    const PRIME: u64 = 0x100_0000_01b3;
    for value in [
        record.kind as u64,
        record.slot.0 as u64,
        record.subject.0,
        record.predicate.0,
        record.object.0,
        record.previous_visible as u64,
        record.resulting_visible as u64,
    ] {
        hash ^= value;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Database, IntentFact, MemoryCommitStore, QueuedIntent, Validator, derive_epoch_world,
    };
    use forthdb_core::{
        Atom, EntityId, Fact, Literal, Pattern, Predicate, PredicateTerm, QueryOptions, SlotId,
        Term, Variable,
    };
    use std::sync::Arc;

    fn define_program(slot: u32, value: u64) -> IntentProgram {
        IntentProgram::new(
            1,
            vec![
                Instruction::allocate(),
                Instruction::store_local(0),
                Instruction::load_local(0),
                Instruction::push(Cell(7)),
                Instruction::push(Cell(value)),
                Instruction::define(SlotToken(slot)),
            ],
        )
    }

    #[test]
    fn rejected_trial_restores_records_deltas_stack_and_allocator() {
        let mut workspace = Workspace::with_capacity(4, 16, 16, 16);
        let rejected = IntentProgram::new(
            1,
            vec![
                Instruction::allocate(),
                Instruction::store_local(0),
                Instruction::load_local(0),
                Instruction::push(Cell(7)),
                Instruction::push(Cell(99)),
                Instruction::define(SlotToken(0)),
                Instruction::reject(),
            ],
        );
        assert_eq!(
            workspace.execute(&rejected),
            ExecutionOutcome::Rejected(ExecError::ExplicitRejection)
        );
        assert_eq!(workspace.record_count(), 0);
        assert_eq!(workspace.delta_count(), 0);
        assert_eq!(workspace.next_entity(), 1);
        assert_eq!(workspace.resolve_object(SlotToken(0)), None);

        assert_eq!(
            workspace.execute(&define_program(0, 100)),
            ExecutionOutcome::Accepted
        );
        assert_eq!(workspace.next_entity(), 2);
        assert_eq!(workspace.resolve_object(SlotToken(0)), Some(Cell(100)));
    }

    #[test]
    fn forget_reveals_the_previous_definition() {
        let mut workspace = Workspace::with_capacity(2, 16, 16, 16);
        assert_eq!(
            workspace.execute(&define_program(0, 1)),
            ExecutionOutcome::Accepted
        );
        assert_eq!(
            workspace.execute(&define_program(0, 2)),
            ExecutionOutcome::Accepted
        );
        assert_eq!(workspace.resolve_object(SlotToken(0)), Some(Cell(2)));

        let forget = IntentProgram::new(0, vec![Instruction::forget(SlotToken(0))]);
        assert_eq!(workspace.execute(&forget), ExecutionOutcome::Accepted);
        assert_eq!(workspace.resolve_object(SlotToken(0)), Some(Cell(1)));
    }

    #[test]
    fn frame_locals_support_multiple_temporary_entities() {
        let mut workspace = Workspace::with_capacity(2, 16, 16, 16);
        let program = IntentProgram::new(
            2,
            vec![
                Instruction::allocate(),
                Instruction::store_local(0),
                Instruction::allocate(),
                Instruction::store_local(1),
                Instruction::load_local(0),
                Instruction::push(Cell(7)),
                Instruction::load_local(1),
                Instruction::define(SlotToken(0)),
            ],
        );
        assert_eq!(workspace.execute(&program), ExecutionOutcome::Accepted);
        assert_eq!(workspace.resolve_object(SlotToken(0)), Some(Cell(2)));
        assert_eq!(workspace.next_entity(), 3);
    }

    #[test]
    fn precondition_observes_prior_accepted_intents_and_current_trial() {
        let mut workspace = Workspace::with_capacity(2, 16, 16, 16);
        assert_eq!(
            workspace.execute(&define_program(0, 10)),
            ExecutionOutcome::Accepted
        );
        let update = IntentProgram::new(
            0,
            vec![
                Instruction::expect_object(SlotToken(0), Cell(10)),
                Instruction::push(Cell(1)),
                Instruction::push(Cell(7)),
                Instruction::push(Cell(11)),
                Instruction::define(SlotToken(0)),
                Instruction::expect_object(SlotToken(0), Cell(11)),
            ],
        );
        assert_eq!(workspace.execute(&update), ExecutionOutcome::Accepted);
        assert_eq!(workspace.resolve_object(SlotToken(0)), Some(Cell(11)));
    }

    fn current_value(world: &crate::World, slot: usize) -> Option<u64> {
        world
            .resolve(&SlotId::new(format!("differential/{slot}")))
            .and_then(|fact| match &fact.object {
                Atom::Literal(value) => value.as_str().parse().ok(),
                Atom::Entity(_) => None,
            })
    }

    fn current_query_slots(
        world: &crate::World,
        fact: &forthdb_core::Fact,
        bound_subject: bool,
        bound_predicate: bool,
        bound_object: bool,
    ) -> Vec<u32> {
        let variable = |name| Variable::new(name).expect("valid variable");
        let pattern = Pattern::new(
            if bound_subject {
                Term::Atom(fact.subject.clone())
            } else {
                Term::Variable(variable("subject"))
            },
            if bound_predicate {
                PredicateTerm::Predicate(fact.predicate.clone())
            } else {
                PredicateTerm::Variable(variable("predicate"))
            },
            if bound_object {
                Term::Atom(fact.object.clone())
            } else {
                Term::Variable(variable("object"))
            },
        );
        let mut slots = world
            .query(
                &[pattern],
                QueryOptions {
                    distinct: false,
                    include_provenance: true,
                    ..QueryOptions::default()
                },
            )
            .rows
            .into_iter()
            .map(|row| {
                row.provenance[0]
                    .as_str()
                    .strip_prefix("differential/")
                    .expect("differential slot")
                    .parse::<u32>()
                    .expect("numeric slot")
                    + 1
            })
            .collect::<Vec<_>>();
        slots.sort_unstable();
        slots
    }

    fn assert_all_index_shapes(world: &crate::World, workspace: &Workspace, root: WorldRoot) {
        let Some((slot, fact)) = (0..64).find_map(|slot| {
            world
                .resolve(&SlotId::new(format!("differential/{slot}")))
                .map(|fact| (slot, fact))
        }) else {
            return;
        };
        let subject = match fact.subject {
            Atom::Entity(entity) => Cell(entity.value()),
            Atom::Literal(_) => panic!("differential subjects are entities"),
        };
        let predicate = Cell(7);
        let object = match &fact.object {
            Atom::Literal(value) => Cell(value.as_str().parse().expect("numeric object")),
            Atom::Entity(_) => panic!("differential objects are literals"),
        };
        let cases = [
            ((true, false, false), QueryPattern::Subject(subject)),
            ((false, true, false), QueryPattern::Predicate(predicate)),
            ((false, false, true), QueryPattern::Object(object)),
            (
                (true, true, false),
                QueryPattern::SubjectPredicate(subject, predicate),
            ),
            (
                (true, false, true),
                QueryPattern::SubjectObject(subject, object),
            ),
            (
                (false, true, true),
                QueryPattern::PredicateObject(predicate, object),
            ),
            (
                (true, true, true),
                QueryPattern::Exact(subject, predicate, object),
            ),
        ];
        for ((bound_subject, bound_predicate, bound_object), pattern) in cases {
            let expected =
                current_query_slots(world, fact, bound_subject, bound_predicate, bound_object);
            let actual = workspace
                .query_slots(root, pattern)
                .into_iter()
                .map(|slot| slot.0)
                .collect::<Vec<_>>();
            assert_eq!(expected, actual, "query shape mismatch at slot {slot}");
        }
    }

    fn assert_all_layered_index_shapes(world: &crate::World, snapshot: &LayeredSnapshot) {
        let Some((slot, fact)) = (0..64).find_map(|slot| {
            world
                .resolve(&SlotId::new(format!("differential/{slot}")))
                .map(|fact| (slot, fact))
        }) else {
            return;
        };
        let subject = match fact.subject {
            Atom::Entity(entity) => Cell(entity.value()),
            Atom::Literal(_) => panic!("differential subjects are entities"),
        };
        let predicate = Cell(7);
        let object = match &fact.object {
            Atom::Literal(value) => Cell(value.as_str().parse().expect("numeric object")),
            Atom::Entity(_) => panic!("differential objects are literals"),
        };
        let cases = [
            ((true, false, false), QueryPattern::Subject(subject)),
            ((false, true, false), QueryPattern::Predicate(predicate)),
            ((false, false, true), QueryPattern::Object(object)),
            (
                (true, true, false),
                QueryPattern::SubjectPredicate(subject, predicate),
            ),
            (
                (true, false, true),
                QueryPattern::SubjectObject(subject, object),
            ),
            (
                (false, true, true),
                QueryPattern::PredicateObject(predicate, object),
            ),
            (
                (true, true, true),
                QueryPattern::Exact(subject, predicate, object),
            ),
        ];
        for ((bound_subject, bound_predicate, bound_object), pattern) in cases {
            let expected =
                current_query_slots(world, fact, bound_subject, bound_predicate, bound_object);
            let actual = snapshot
                .query_slots(pattern)
                .into_iter()
                .map(|slot| slot.0)
                .collect::<Vec<_>>();
            assert_eq!(expected, actual, "layered query mismatch at slot {slot}");
            assert_eq!(snapshot.query_count(pattern), actual.len());
        }
    }

    #[test]
    fn deterministic_epoch_sweep_matches_current_slot_semantics() {
        for width in [16, 64, 128, 256] {
            let database = Database::new(MemoryCommitStore::new()).expect("genesis is valid");
            let mut world = database.snapshot();
            let mut workspace = Workspace::with_indexes(65, 32, 4096, 4096);
            let reject_slot = SlotId::new("differential/reject");
            let validator_slot = reject_slot.clone();
            let validator: Validator = Arc::new(move |candidate| {
                if candidate.resolve(&validator_slot).is_some() {
                    Err("deliberate differential rejection".to_owned())
                } else {
                    Ok(())
                }
            });
            let validators = [validator];
            let mut first_root = None;
            let mut first_root_value = None;
            let mut final_root = None;

            for epoch in 0..(512 / width) {
                let mut current = Vec::with_capacity(width);
                let mut programs = Vec::with_capacity(width);
                for position in 0..width {
                    let index = epoch * width + position;
                    let slot = index % 64;
                    if index % 19 == 0 {
                        let mut intent = QueuedIntent::new();
                        let entity = intent.entity();
                        intent.define(
                            reject_slot.clone(),
                            IntentFact::new(
                                entity,
                                Predicate::new("state"),
                                Literal::new(index.to_string()),
                            ),
                        );
                        current.push(intent);
                        programs.push(IntentProgram::new(
                            1,
                            vec![
                                Instruction::allocate(),
                                Instruction::store_local(0),
                                Instruction::load_local(0),
                                Instruction::push(Cell(1)),
                                Instruction::push(Cell(index as u64)),
                                Instruction::define(SlotToken(0)),
                                Instruction::reject(),
                            ],
                        ));
                    } else if index % 7 == 0 {
                        let mut intent = QueuedIntent::new();
                        intent.forget(SlotId::new(format!("differential/{slot}")));
                        current.push(intent);
                        programs.push(IntentProgram::new(
                            0,
                            vec![Instruction::forget(SlotToken((slot + 1) as u32))],
                        ));
                    } else if index % 5 == 0 {
                        let subject = (index % 4 + 1) as u64;
                        let object = (index % 8) as u64;
                        let mut intent = QueuedIntent::new();
                        intent.define_fact(
                            SlotId::new(format!("differential/{slot}")),
                            Fact::new(
                                Atom::Entity(EntityId::new(subject)),
                                Predicate::new("state"),
                                Atom::Literal(Literal::new(object.to_string())),
                            ),
                        );
                        current.push(intent);
                        programs.push(IntentProgram::new(
                            0,
                            vec![
                                Instruction::push(Cell(subject)),
                                Instruction::push(Cell(7)),
                                Instruction::push(Cell(object)),
                                Instruction::define(SlotToken((slot + 1) as u32)),
                            ],
                        ));
                    } else {
                        let mut intent = QueuedIntent::new();
                        let entity = intent.entity();
                        intent.define(
                            SlotId::new(format!("differential/{slot}")),
                            IntentFact::new(
                                entity,
                                Predicate::new("state"),
                                Literal::new(index.to_string()),
                            ),
                        );
                        current.push(intent);
                        programs.push(define_program((slot + 1) as u32, index as u64));
                    }
                }

                let plan = derive_epoch_world(world, current, &validators);
                for (outcome, program) in plan.outcomes().iter().zip(&programs) {
                    let actual = workspace.execute(program);
                    assert_eq!(
                        outcome.accepted().is_some(),
                        actual == ExecutionOutcome::Accepted,
                        "width {width}, epoch {epoch}"
                    );
                }
                world = plan.tail();
                let root = workspace
                    .publish_epoch()
                    .expect("epoch has accepted effects");
                final_root = Some(root);
                assert_eq!(root.allocator_head(), world.next_entity());
                assert_eq!(root.version(), (epoch + 1) as u64);
                assert_ne!(root.world_id(), 0);
                assert_all_index_shapes(&world, &workspace, root);
                if first_root.is_none() {
                    first_root = Some(root);
                    first_root_value = Some(workspace.resolve_object_at(root, SlotToken(2)));
                }
                assert_eq!(world.next_entity(), workspace.next_entity());
                for slot in 0..64 {
                    assert_eq!(
                        current_value(&world, slot),
                        workspace
                            .resolve_object(SlotToken((slot + 1) as u32))
                            .map(|cell| cell.0),
                        "width {width}, epoch {epoch}, slot {slot}"
                    );
                }
            }
            assert_eq!(world.active_slot_count(), workspace.active_slot_count());
            let compact = Arc::new(workspace.compact_base(first_root.unwrap()));
            let snapshot = workspace.layered_snapshot(compact, final_root.unwrap());
            assert_eq!(snapshot.world_id(), final_root.unwrap().world_id());
            assert_all_layered_index_shapes(&world, &snapshot);
            assert_eq!(
                workspace.resolve_object_at(first_root.unwrap(), SlotToken(2)),
                first_root_value.unwrap(),
                "later epochs must not change an older root"
            );
            assert_eq!(
                world.resolve(&reject_slot),
                None,
                "rejected marker must remain invisible"
            );
        }
    }
}
