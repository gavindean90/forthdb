//! Experimental in-memory semantic VM for the Phase 1 arena/stack benchmark.
//!
//! This deliberately does not change the durable admission format. It isolates
//! the cost of materializing rejectable intents without cloning `ForthDb`.

const NONE: u32 = u32::MAX;

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Cell(pub u64);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlotToken(pub u32);

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Opcode {
    ExpectObject,
    Allocate,
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
    accepted_heads: Vec<u32>,
    next_entity: u64,
}

impl Workspace {
    pub fn with_capacity(
        slot_count: usize,
        stack_capacity: usize,
        record_capacity: usize,
        delta_capacity: usize,
    ) -> Self {
        Self {
            stack: vec![Cell::default(); stack_capacity],
            stack_pointer: 0,
            frame_base: 0,
            records: PodArena::with_capacity(record_capacity),
            deltas: PodArena::with_capacity(delta_capacity),
            accepted_heads: vec![NONE; slot_count],
            next_entity: 1,
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

    pub fn next_entity(&self) -> u64 {
        self.next_entity
    }

    pub fn record_count(&self) -> usize {
        self.records.frontier()
    }

    pub fn delta_count(&self) -> usize {
        self.deltas.frontier()
    }

    pub fn active_slot_count(&self) -> usize {
        self.accepted_heads
            .iter()
            .filter(|head| **head != NONE)
            .count()
    }

    fn checkpoint(&self) -> TrialCheckpoint {
        TrialCheckpoint {
            stack_pointer: self.stack_pointer,
            frame_base: self.frame_base,
            record_frontier: self.records.frontier(),
            delta_frontier: self.deltas.frontier(),
            next_entity: self.next_entity,
        }
    }

    #[inline]
    fn rollback(&mut self, checkpoint: TrialCheckpoint) {
        self.stack_pointer = checkpoint.stack_pointer;
        self.frame_base = checkpoint.frame_base;
        self.records.restore(checkpoint.record_frontier);
        self.deltas.restore(checkpoint.delta_frontier);
        self.next_entity = checkpoint.next_entity;
    }

    fn accept(&mut self, checkpoint: TrialCheckpoint) {
        for delta in &self.deltas.active()[checkpoint.delta_frontier..] {
            self.accepted_heads[delta.slot.0 as usize] = delta.resulting_visible;
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
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Database, IntentFact, MemoryCommitStore, QueuedIntent, Validator, derive_epoch_world,
    };
    use forthdb_core::{Atom, Literal, Predicate, SlotId};
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

    #[test]
    fn deterministic_epoch_sweep_matches_current_slot_semantics() {
        for width in [16, 64, 128, 256] {
            let database = Database::new(MemoryCommitStore::new()).expect("genesis is valid");
            let mut world = database.snapshot();
            let mut workspace = Workspace::with_capacity(65, 32, 4096, 4096);
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
            assert_eq!(
                world.resolve(&reject_slot),
                None,
                "rejected marker must remain invisible"
            );
        }
    }
}
