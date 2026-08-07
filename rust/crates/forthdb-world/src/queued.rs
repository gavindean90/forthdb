use super::*;
use crate::mmap_vm_snapshot::MmapVmSnapshot;
use crate::semantic_isa::{InstructionStreamFrame, StreamDictionary};
use crate::stack_vm::{
    Cell, ExecutionOutcome, Instruction, IntentProgram, Opcode, SlotToken, Workspace as VmWorkspace,
};
use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_INTENT_NAMESPACE: AtomicU64 = AtomicU64::new(1);

/// An entity handle local to exactly one queued intent.
///
/// The namespace is intentionally opaque. Two intents may both allocate
/// temporary index zero, but their handles remain distinct and cannot alias.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TempEntity {
    namespace: u64,
    index: u32,
}

impl TempEntity {
    pub const fn index(self) -> u32 {
        self.index
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum IntentAtom {
    Entity(EntityId),
    Temporary(TempEntity),
    Literal(Literal),
}

impl From<EntityId> for IntentAtom {
    fn from(value: EntityId) -> Self {
        Self::Entity(value)
    }
}

impl From<TempEntity> for IntentAtom {
    fn from(value: TempEntity) -> Self {
        Self::Temporary(value)
    }
}

impl From<Literal> for IntentAtom {
    fn from(value: Literal) -> Self {
        Self::Literal(value)
    }
}

impl From<Atom> for IntentAtom {
    fn from(value: Atom) -> Self {
        match value {
            Atom::Entity(entity) => Self::Entity(entity),
            Atom::Literal(literal) => Self::Literal(literal),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct IntentFact {
    pub subject: IntentAtom,
    pub predicate: Predicate,
    pub object: IntentAtom,
}

impl IntentFact {
    pub fn new(
        subject: impl Into<IntentAtom>,
        predicate: Predicate,
        object: impl Into<IntentAtom>,
    ) -> Self {
        Self {
            subject: subject.into(),
            predicate,
            object: object.into(),
        }
    }
}

impl From<Fact> for IntentFact {
    fn from(value: Fact) -> Self {
        Self {
            subject: value.subject.into(),
            predicate: value.predicate,
            object: value.object.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IntentPrecondition {
    ExpectedWorld(WorldId),
    ExpectedSlot {
        slot: SlotId,
        expected: Option<Fact>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum IntentOperation {
    AllocateEntity { temporary: TempEntity },
    Define { slot: SlotId, fact: IntentFact },
    Forget { slot: SlotId },
}

/// Operations that delegate predecessor assignment to an epoch planner.
///
/// This is deliberately not a [`Transaction`]. A strict transaction retains
/// an absolute base world and the existing stale-writer contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedIntent {
    namespace: u64,
    next_temporary: u32,
    preconditions: Vec<IntentPrecondition>,
    operations: Vec<IntentOperation>,
}

impl Default for QueuedIntent {
    fn default() -> Self {
        Self {
            namespace: NEXT_INTENT_NAMESPACE.fetch_add(1, Ordering::Relaxed),
            next_temporary: 0,
            preconditions: Vec::new(),
            operations: Vec::new(),
        }
    }
}

impl QueuedIntent {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn operation_count(&self) -> usize {
        self.operations.len()
    }

    pub fn precondition_count(&self) -> usize {
        self.preconditions.len()
    }

    pub fn expect_world(&mut self, world: WorldId) {
        self.preconditions
            .push(IntentPrecondition::ExpectedWorld(world));
    }

    pub fn expect_value(&mut self, slot: SlotId, fact: Fact) {
        self.preconditions.push(IntentPrecondition::ExpectedSlot {
            slot,
            expected: Some(fact),
        });
    }

    pub fn expect_absent(&mut self, slot: SlotId) {
        self.preconditions.push(IntentPrecondition::ExpectedSlot {
            slot,
            expected: None,
        });
    }

    pub fn entity(&mut self) -> TempEntity {
        let temporary = TempEntity {
            namespace: self.namespace,
            index: self.next_temporary,
        };
        self.next_temporary = self
            .next_temporary
            .checked_add(1)
            .expect("queued intent temporary-entity identifier overflow");
        self.operations
            .push(IntentOperation::AllocateEntity { temporary });
        temporary
    }

    pub fn define(&mut self, slot: SlotId, fact: IntentFact) {
        self.operations.push(IntentOperation::Define { slot, fact });
    }

    pub fn define_fact(&mut self, slot: SlotId, fact: Fact) {
        self.define(slot, fact.into());
    }

    pub fn forget(&mut self, slot: SlotId) {
        self.operations.push(IntentOperation::Forget { slot });
    }

    pub fn compile_to_stream_frame(&self) -> Result<InstructionStreamFrame, String> {
        let mut slots = BTreeMap::<SlotId, SlotToken>::new();
        let mut slot_vec = Vec::new();

        let mut predicates = BTreeMap::<Predicate, Cell>::new();
        let mut pred_vec = Vec::new();

        let mut literals = BTreeMap::<Literal, Cell>::new();
        let mut lit_vec = Vec::new();

        let mut get_slot = |slot: &SlotId| -> SlotToken {
            if let Some(&token) = slots.get(slot) {
                token
            } else {
                let token = SlotToken(slots.len() as u32);
                slots.insert(slot.clone(), token);
                slot_vec.push((token, slot.clone()));
                token
            }
        };

        let mut get_predicate = |pred: &Predicate| -> Cell {
            if let Some(&cell) = predicates.get(pred) {
                cell
            } else {
                let cell = Cell(predicates.len() as u64);
                predicates.insert(pred.clone(), cell);
                pred_vec.push((cell, pred.clone()));
                cell
            }
        };

        let mut get_literal = |lit: &Literal| -> Cell {
            if let Some(&cell) = literals.get(lit) {
                cell
            } else {
                let cell = Cell(VM_LITERAL_BASE + literals.len() as u64);
                literals.insert(lit.clone(), cell);
                lit_vec.push((cell, lit.clone()));
                cell
            }
        };

        let mut instructions = Vec::new();

        for prec in &self.preconditions {
            match prec {
                IntentPrecondition::ExpectedWorld(world) => {
                    instructions.push(Instruction::raw(
                        Opcode::ExpectObject,
                        u32::MAX,
                        world.value(),
                    ));
                }
                IntentPrecondition::ExpectedSlot { slot, expected } => {
                    let slot_token = get_slot(slot);
                    match expected {
                        Some(fact) => {
                            match &fact.subject {
                                Atom::Entity(e) => {
                                    instructions.push(Instruction::push(Cell(e.value())))
                                }
                                Atom::Literal(l) => {
                                    instructions.push(Instruction::push(get_literal(l)))
                                }
                            }
                            instructions.push(Instruction::push(get_predicate(&fact.predicate)));
                            match &fact.object {
                                Atom::Entity(e) => {
                                    instructions.push(Instruction::push(Cell(e.value())))
                                }
                                Atom::Literal(l) => {
                                    instructions.push(Instruction::push(get_literal(l)))
                                }
                            }
                            instructions.push(Instruction::expect_object(slot_token, Cell(1)));
                        }
                        None => {
                            instructions.push(Instruction::expect_object(slot_token, Cell(0)));
                        }
                    }
                }
            }
        }

        for op in &self.operations {
            match op {
                IntentOperation::AllocateEntity { temporary } => {
                    instructions.push(Instruction::allocate());
                    instructions.push(Instruction::store_local(temporary.index));
                }
                IntentOperation::Define { slot, fact } => {
                    let slot_token = get_slot(slot);

                    match &fact.subject {
                        IntentAtom::Entity(e) => {
                            instructions.push(Instruction::push(Cell(e.value())))
                        }
                        IntentAtom::Temporary(t) => {
                            instructions.push(Instruction::load_local(t.index))
                        }
                        IntentAtom::Literal(l) => {
                            instructions.push(Instruction::push(get_literal(l)))
                        }
                    }
                    instructions.push(Instruction::push(get_predicate(&fact.predicate)));
                    match &fact.object {
                        IntentAtom::Entity(e) => {
                            instructions.push(Instruction::push(Cell(e.value())))
                        }
                        IntentAtom::Temporary(t) => {
                            instructions.push(Instruction::load_local(t.index))
                        }
                        IntentAtom::Literal(l) => {
                            instructions.push(Instruction::push(get_literal(l)))
                        }
                    }

                    instructions.push(Instruction::define(slot_token));
                }
                IntentOperation::Forget { slot } => {
                    let slot_token = get_slot(slot);
                    instructions.push(Instruction::forget(slot_token));
                }
            }
        }

        let dictionary = StreamDictionary {
            slots: slot_vec,
            predicates: pred_vec,
            literals: lit_vec,
        };

        Ok(InstructionStreamFrame::new(
            self.namespace,
            dictionary,
            self.next_temporary,
            instructions,
        ))
    }

    pub fn from_stream_frame(frame: &InstructionStreamFrame) -> Result<Self, String> {
        let mut slot_map = BTreeMap::<u32, SlotId>::new();
        for (token, slot) in &frame.dictionary.slots {
            slot_map.insert(token.0, slot.clone());
        }

        let mut pred_map = BTreeMap::<u64, Predicate>::new();
        for (cell, pred) in &frame.dictionary.predicates {
            pred_map.insert(cell.0, pred.clone());
        }

        let mut lit_map = BTreeMap::<u64, Literal>::new();
        for (cell, lit) in &frame.dictionary.literals {
            lit_map.insert(cell.0, lit.clone());
        }

        #[derive(Clone, Debug)]
        enum StackVal {
            Atom(IntentAtom),
            CellVal(u64),
        }

        let mut intent = QueuedIntent::new();
        intent.namespace = frame.namespace;
        let mut stack = Vec::<StackVal>::new();

        let resolve_atom = |val: &StackVal, lit_map: &BTreeMap<u64, Literal>| -> IntentAtom {
            match val {
                StackVal::Atom(a) => a.clone(),
                StackVal::CellVal(c) => {
                    if *c < VM_LITERAL_BASE {
                        IntentAtom::Entity(EntityId::new(*c))
                    } else if let Some(lit) = lit_map.get(c) {
                        IntentAtom::Literal(lit.clone())
                    } else {
                        IntentAtom::Literal(Literal::new(&format!("lit_{c}")))
                    }
                }
            }
        };

        let mut idx = 0;
        let instructions = frame.instructions();

        while idx < instructions.len() {
            let inst = instructions[idx];
            match inst.opcode() {
                Opcode::Allocate => {
                    let temp = intent.entity();
                    if idx + 1 < instructions.len()
                        && instructions[idx + 1].opcode() == Opcode::StoreLocal
                    {
                        idx += 1;
                    }
                }
                Opcode::AllocateDiscard => {
                    intent.entity();
                }
                Opcode::PushCell => {
                    stack.push(StackVal::CellVal(inst.immediate()));
                }
                Opcode::LoadLocal => {
                    let temp = TempEntity {
                        namespace: intent.namespace,
                        index: inst.argument(),
                    };
                    stack.push(StackVal::Atom(IntentAtom::Temporary(temp)));
                }
                Opcode::Define => {
                    if stack.len() >= 3 {
                        let obj_val = stack.pop().unwrap();
                        let pred_val = stack.pop().unwrap();
                        let subj_val = stack.pop().unwrap();

                        let predicate = match pred_val {
                            StackVal::CellVal(c) => pred_map
                                .get(&c)
                                .cloned()
                                .unwrap_or_else(|| Predicate::new(&format!("pred_{c}"))),
                            StackVal::Atom(IntentAtom::Literal(l)) => Predicate::new(l.as_str()),
                            _ => Predicate::new("predicate"),
                        };

                        let subject = resolve_atom(&subj_val, &lit_map);
                        let object = resolve_atom(&obj_val, &lit_map);

                        let slot_id = slot_map
                            .get(&inst.argument())
                            .cloned()
                            .ok_or_else(|| format!("unknown slot token {}", inst.argument()))?;

                        let fact = IntentFact {
                            subject,
                            predicate,
                            object,
                        };
                        intent.define(slot_id, fact);
                    }
                }
                Opcode::Forget => {
                    let slot_id = slot_map
                        .get(&inst.argument())
                        .cloned()
                        .ok_or_else(|| format!("unknown slot token {}", inst.argument()))?;
                    intent.forget(slot_id);
                }
                Opcode::ExpectObject => {
                    if inst.argument() == u32::MAX {
                        intent.expect_world(WorldId::new(inst.immediate()));
                    } else {
                        let slot_id = slot_map
                            .get(&inst.argument())
                            .cloned()
                            .ok_or_else(|| format!("unknown slot token {}", inst.argument()))?;

                        if inst.immediate() == 0 {
                            intent.expect_absent(slot_id);
                        } else if stack.len() >= 3 {
                            let obj_val = stack.pop().unwrap();
                            let pred_val = stack.pop().unwrap();
                            let subj_val = stack.pop().unwrap();

                            let predicate = match pred_val {
                                StackVal::CellVal(c) => pred_map
                                    .get(&c)
                                    .cloned()
                                    .unwrap_or_else(|| Predicate::new(&format!("pred_{c}"))),
                                StackVal::Atom(IntentAtom::Literal(l)) => {
                                    Predicate::new(l.as_str())
                                }
                                _ => Predicate::new("predicate"),
                            };

                            let subj_atom = resolve_atom(&subj_val, &lit_map);
                            let obj_atom = resolve_atom(&obj_val, &lit_map);

                            let subject = match subj_atom {
                                IntentAtom::Entity(e) => Atom::Entity(e),
                                IntentAtom::Literal(l) => Atom::Literal(l),
                                IntentAtom::Temporary(_) => Atom::Entity(EntityId::new(0)),
                            };

                            let object = match obj_atom {
                                IntentAtom::Entity(e) => Atom::Entity(e),
                                IntentAtom::Literal(l) => Atom::Literal(l),
                                IntentAtom::Temporary(_) => Atom::Entity(EntityId::new(0)),
                            };

                            intent.expect_value(slot_id, Fact::new(subject, predicate, object));
                        }
                    }
                }
                _ => {}
            }
            idx += 1;
        }

        Ok(intent)
    }
}

pub(crate) fn encode_queued_intent(intent: &QueuedIntent, output: &mut Vec<u8>) {
    put_u64(output, intent.namespace);
    put_u32(output, intent.next_temporary);
    put_u32(output, intent.preconditions.len() as u32);
    for precondition in &intent.preconditions {
        match precondition {
            IntentPrecondition::ExpectedWorld(world) => {
                output.push(0);
                put_u64(output, world.value());
            }
            IntentPrecondition::ExpectedSlot { slot, expected } => {
                output.push(1);
                put_string(output, slot.as_str());
                match expected {
                    Some(fact) => {
                        output.push(1);
                        encode_fact(output, fact);
                    }
                    None => output.push(0),
                }
            }
        }
    }
    put_u32(output, intent.operations.len() as u32);
    for operation in &intent.operations {
        match operation {
            IntentOperation::AllocateEntity { temporary } => {
                output.push(0);
                encode_temporary(output, *temporary);
            }
            IntentOperation::Define { slot, fact } => {
                output.push(1);
                put_string(output, slot.as_str());
                encode_intent_fact(output, fact);
            }
            IntentOperation::Forget { slot } => {
                output.push(2);
                put_string(output, slot.as_str());
            }
        }
    }
}

pub(crate) fn decode_queued_intent(input: &mut Cursor<&[u8]>) -> Result<QueuedIntent, String> {
    let namespace = take_u64(input)?;
    let next_temporary = take_u32(input)?;
    let precondition_count = take_u32(input)? as usize;
    let mut preconditions = Vec::with_capacity(precondition_count);
    for _ in 0..precondition_count {
        preconditions.push(match take_u8(input)? {
            0 => IntentPrecondition::ExpectedWorld(WorldId::new(take_u64(input)?)),
            1 => {
                let slot = SlotId::new(take_string(input)?);
                let expected = match take_u8(input)? {
                    0 => None,
                    1 => Some(decode_fact(input)?),
                    tag => return Err(format!("invalid expected-fact tag {tag}")),
                };
                IntentPrecondition::ExpectedSlot { slot, expected }
            }
            tag => return Err(format!("invalid intent precondition tag {tag}")),
        });
    }
    let operation_count = take_u32(input)? as usize;
    let mut operations = Vec::with_capacity(operation_count);
    for _ in 0..operation_count {
        operations.push(match take_u8(input)? {
            0 => IntentOperation::AllocateEntity {
                temporary: decode_temporary(input)?,
            },
            1 => IntentOperation::Define {
                slot: SlotId::new(take_string(input)?),
                fact: decode_intent_fact(input)?,
            },
            2 => IntentOperation::Forget {
                slot: SlotId::new(take_string(input)?),
            },
            tag => return Err(format!("invalid intent operation tag {tag}")),
        });
    }
    NEXT_INTENT_NAMESPACE.fetch_max(namespace.saturating_add(1), Ordering::Relaxed);
    Ok(QueuedIntent {
        namespace,
        next_temporary,
        preconditions,
        operations,
    })
}

fn encode_temporary(output: &mut Vec<u8>, temporary: TempEntity) {
    put_u64(output, temporary.namespace);
    put_u32(output, temporary.index);
}

fn decode_temporary(input: &mut Cursor<&[u8]>) -> Result<TempEntity, String> {
    Ok(TempEntity {
        namespace: take_u64(input)?,
        index: take_u32(input)?,
    })
}

fn encode_intent_fact(output: &mut Vec<u8>, fact: &IntentFact) {
    encode_intent_atom(output, &fact.subject);
    put_string(output, fact.predicate.as_str());
    encode_intent_atom(output, &fact.object);
}

fn decode_intent_fact(input: &mut Cursor<&[u8]>) -> Result<IntentFact, String> {
    Ok(IntentFact {
        subject: decode_intent_atom(input)?,
        predicate: Predicate::new(take_string(input)?),
        object: decode_intent_atom(input)?,
    })
}

fn encode_intent_atom(output: &mut Vec<u8>, atom: &IntentAtom) {
    match atom {
        IntentAtom::Entity(entity) => {
            output.push(0);
            put_u64(output, entity.value());
        }
        IntentAtom::Temporary(temporary) => {
            output.push(1);
            encode_temporary(output, *temporary);
        }
        IntentAtom::Literal(literal) => {
            output.push(2);
            put_string(output, literal.as_str());
        }
    }
}

fn decode_intent_atom(input: &mut Cursor<&[u8]>) -> Result<IntentAtom, String> {
    match take_u8(input)? {
        0 => Ok(IntentAtom::Entity(EntityId::new(take_u64(input)?))),
        1 => Ok(IntentAtom::Temporary(decode_temporary(input)?)),
        2 => Ok(IntentAtom::Literal(Literal::new(take_string(input)?))),
        tag => Err(format!("invalid intent atom tag {tag}")),
    }
}

fn encode_fact(output: &mut Vec<u8>, fact: &Fact) {
    encode_atom(output, &fact.subject);
    put_string(output, fact.predicate.as_str());
    encode_atom(output, &fact.object);
}

fn decode_fact(input: &mut Cursor<&[u8]>) -> Result<Fact, String> {
    Ok(Fact::new(
        decode_atom(input)?,
        Predicate::new(take_string(input)?),
        decode_atom(input)?,
    ))
}

fn encode_atom(output: &mut Vec<u8>, atom: &Atom) {
    match atom {
        Atom::Entity(entity) => {
            output.push(0);
            put_u64(output, entity.value());
        }
        Atom::Literal(literal) => {
            output.push(1);
            put_string(output, literal.as_str());
        }
    }
}

fn decode_atom(input: &mut Cursor<&[u8]>) -> Result<Atom, String> {
    match take_u8(input)? {
        0 => Ok(Atom::Entity(EntityId::new(take_u64(input)?))),
        1 => Ok(Atom::Literal(Literal::new(take_string(input)?))),
        tag => Err(format!("invalid fact atom tag {tag}")),
    }
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_string(output: &mut Vec<u8>, value: &str) {
    put_u32(output, value.len() as u32);
    output.extend_from_slice(value.as_bytes());
}

fn take_u8(input: &mut Cursor<&[u8]>) -> Result<u8, String> {
    let mut bytes = [0; 1];
    input
        .read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(bytes[0])
}

fn take_u32(input: &mut Cursor<&[u8]>) -> Result<u32, String> {
    let mut bytes = [0; 4];
    input
        .read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(u32::from_le_bytes(bytes))
}

fn take_u64(input: &mut Cursor<&[u8]>) -> Result<u64, String> {
    let mut bytes = [0; 8];
    input
        .read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(u64::from_le_bytes(bytes))
}

fn take_string(input: &mut Cursor<&[u8]>) -> Result<String, String> {
    let length = take_u32(input)? as usize;
    let start = input.position() as usize;
    let end = start
        .checked_add(length)
        .ok_or_else(|| "intent string length overflow".to_owned())?;
    let bytes = input.get_ref();
    if end > bytes.len() {
        return Err("truncated intent string".to_owned());
    }
    let value = std::str::from_utf8(&bytes[start..end])
        .map_err(|error| error.to_string())?
        .to_owned();
    input.set_position(end as u64);
    Ok(value)
}

#[derive(Debug)]
pub enum IntentRejection {
    WorldPrecondition {
        expected: WorldId,
        actual: WorldId,
    },
    SlotPrecondition {
        slot: SlotId,
        expected: Option<Fact>,
        actual: Option<Fact>,
    },
    UnknownTemporaryEntity(TempEntity),
    Candidate(CandidateError),
    Validation(String),
}

impl fmt::Display for IntentRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorldPrecondition { expected, actual } => write!(
                formatter,
                "queued intent expected predecessor {expected}, found {actual}"
            ),
            Self::SlotPrecondition {
                slot,
                expected,
                actual,
            } => write!(
                formatter,
                "queued intent slot precondition failed for {slot:?}: expected {expected:?}, found {actual:?}"
            ),
            Self::UnknownTemporaryEntity(entity) => write!(
                formatter,
                "queued intent referenced temporary entity {} from another scope or before allocation",
                entity.index()
            ),
            Self::Candidate(error) => write!(formatter, "queued candidate failed: {error}"),
            Self::Validation(message) => {
                write!(formatter, "queued candidate validation failed: {message}")
            }
        }
    }
}

impl Error for IntentRejection {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Candidate(error) => Some(error),
            Self::WorldPrecondition { .. }
            | Self::SlotPrecondition { .. }
            | Self::UnknownTemporaryEntity(_)
            | Self::Validation(_) => None,
        }
    }
}

#[derive(Debug)]
pub struct AcceptedIntent {
    position: usize,
    world: Arc<World>,
    frame: Arc<CommitFrame>,
    entities: BTreeMap<TempEntity, EntityId>,
}

impl AcceptedIntent {
    pub fn position(&self) -> usize {
        self.position
    }

    pub fn world(&self) -> Arc<World> {
        self.world.clone()
    }

    pub fn frame(&self) -> Arc<CommitFrame> {
        self.frame.clone()
    }

    pub fn entity(&self, temporary: TempEntity) -> Option<EntityId> {
        self.entities.get(&temporary).copied()
    }

    pub fn entities(&self) -> &BTreeMap<TempEntity, EntityId> {
        &self.entities
    }
}

#[derive(Debug)]
pub struct RejectedIntent {
    position: usize,
    error: IntentRejection,
}

impl RejectedIntent {
    pub fn position(&self) -> usize {
        self.position
    }

    pub fn error(&self) -> &IntentRejection {
        &self.error
    }
}

#[derive(Debug)]
pub enum EpochOutcome {
    Accepted(AcceptedIntent),
    Rejected(RejectedIntent),
}

impl EpochOutcome {
    pub fn position(&self) -> usize {
        match self {
            Self::Accepted(accepted) => accepted.position(),
            Self::Rejected(rejected) => rejected.position(),
        }
    }

    pub fn accepted(&self) -> Option<&AcceptedIntent> {
        match self {
            Self::Accepted(accepted) => Some(accepted),
            Self::Rejected(_) => None,
        }
    }

    pub fn rejected(&self) -> Option<&RejectedIntent> {
        match self {
            Self::Accepted(_) => None,
            Self::Rejected(rejected) => Some(rejected),
        }
    }
}

#[derive(Debug)]
pub struct AcceptedSemanticIntent {
    position: usize,
    world: Arc<World>,
    frame: Arc<CommitFrame>,
    bindings: crate::transaction_ast::SemanticBindings,
}

impl AcceptedSemanticIntent {
    pub fn new(
        position: usize,
        world: Arc<World>,
        frame: Arc<CommitFrame>,
        bindings: crate::transaction_ast::SemanticBindings,
    ) -> Self {
        Self {
            position,
            world,
            frame,
            bindings,
        }
    }

    pub fn position(&self) -> usize {
        self.position
    }
    pub fn world(&self) -> Arc<World> {
        self.world.clone()
    }
    pub fn frame(&self) -> Arc<CommitFrame> {
        self.frame.clone()
    }
    pub fn bindings(&self) -> &crate::transaction_ast::SemanticBindings {
        &self.bindings
    }
    pub fn into_bindings(self) -> crate::transaction_ast::SemanticBindings {
        self.bindings
    }
}

#[derive(Debug)]
pub struct RejectedSemanticIntent {
    position: usize,
    error: crate::queued_controller::SemanticTicketRejection,
}

impl RejectedSemanticIntent {
    pub fn new(position: usize, error: crate::queued_controller::SemanticTicketRejection) -> Self {
        Self { position, error }
    }

    pub fn position(&self) -> usize {
        self.position
    }
    pub fn error(&self) -> &crate::queued_controller::SemanticTicketRejection {
        &self.error
    }
    pub fn into_error(self) -> crate::queued_controller::SemanticTicketRejection {
        self.error
    }
}

#[derive(Debug)]
pub enum MixedEpochOutcome {
    QueuedAccepted(AcceptedIntent),
    QueuedRejected(RejectedIntent),
    SemanticAccepted(AcceptedSemanticIntent),
    SemanticRejected(RejectedSemanticIntent),
}

#[derive(Debug)]
pub struct MixedEpochPlan {
    base: Arc<World>,
    tail: Arc<World>,
    outcomes: Vec<MixedEpochOutcome>,
    frames: Vec<Arc<CommitFrame>>,
}

impl MixedEpochPlan {
    pub fn new(
        base: Arc<World>,
        tail: Arc<World>,
        outcomes: Vec<MixedEpochOutcome>,
        frames: Vec<Arc<CommitFrame>>,
    ) -> Self {
        Self {
            base,
            tail,
            outcomes,
            frames,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
    pub fn tail(&self) -> Arc<World> {
        self.tail.clone()
    }
    pub fn frames(&self) -> &[Arc<CommitFrame>] {
        &self.frames
    }
    pub fn outcomes(&self) -> &[MixedEpochOutcome] {
        &self.outcomes
    }
    pub fn into_outcomes(self) -> Vec<MixedEpochOutcome> {
        self.outcomes
    }
}

/// A deterministic private successor chain. Derivation never mutates the base
/// and never publishes a successor.
#[derive(Debug)]
pub struct EpochPlan {
    base: Arc<World>,
    tail: Arc<World>,
    outcomes: Vec<EpochOutcome>,
    frames: Vec<Arc<CommitFrame>>,
}

impl EpochPlan {
    pub fn base(&self) -> Arc<World> {
        self.base.clone()
    }

    pub fn tail(&self) -> Arc<World> {
        self.tail.clone()
    }

    pub fn outcomes(&self) -> &[EpochOutcome] {
        &self.outcomes
    }

    pub fn frames(&self) -> &[Arc<CommitFrame>] {
        &self.frames
    }

    pub fn accepted_count(&self) -> usize {
        self.frames.len()
    }

    pub fn rejected_count(&self) -> usize {
        self.outcomes.len() - self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

const VM_LITERAL_BASE: u64 = 1 << 63;

/// Persistent token dictionaries and semantic state for the durable VM
/// materializer. Durable intents remain the canonical journal representation;
/// both live execution and recovery compile them through this same state.
#[derive(Clone)]
pub struct VmEpochMaterializer {
    workspace: VmWorkspace,
    version: u64,
    mapped_base: Option<Arc<MmapVmSnapshot>>,
    slots: BTreeMap<SlotId, SlotToken>,
    predicates: BTreeMap<Predicate, Cell>,
    predicate_values: Vec<Predicate>,
    literals: BTreeMap<Literal, Cell>,
    literal_values: Vec<Literal>,
}

impl VmEpochMaterializer {
    pub fn new(next_entity: u64) -> Self {
        Self {
            workspace: VmWorkspace::with_indexes_from(next_entity, 0, 32, 1_024, 1_024),
            version: 0,
            mapped_base: None,
            slots: BTreeMap::new(),
            predicates: BTreeMap::new(),
            predicate_values: Vec::new(),
            literals: BTreeMap::new(),
            literal_values: Vec::new(),
        }
    }

    pub(crate) fn from_mmap(snapshot: Arc<MmapVmSnapshot>) -> Result<Self, String> {
        let (records, heads) = snapshot.workspace_image()?;
        let workspace = VmWorkspace::from_physical_snapshot(
            snapshot.next_entity(),
            records,
            heads,
            snapshot.record_count(),
        )?;
        Ok(Self {
            workspace,
            version: 0,
            mapped_base: Some(snapshot),
            slots: BTreeMap::new(),
            predicates: BTreeMap::new(),
            predicate_values: Vec::new(),
            literals: BTreeMap::new(),
            literal_values: Vec::new(),
        })
    }

    pub(crate) fn from_world(base: &Arc<World>) -> Result<Self, String> {
        let mut materializer = Self::new(base.next_entity());
        materializer.synchronize_to(base)?;
        Ok(materializer)
    }

    pub(crate) fn synchronize_to(&mut self, base: &Arc<World>) -> Result<(), String> {
        if self.version > base.version() {
            return Err("VM materializer cannot rewind".to_owned());
        }
        if self.version == base.version() {
            return Ok(());
        }

        let frames = base.frames();
        if self.version == 0 && base.version() > 0 {
            // Need to apply from beginning
            self.workspace = VmWorkspace::with_indexes_from(1, 0, 32, 1_024, 1_024);
        }

        for frame in frames {
            let frame_version = frame.parent_version() + 1;
            if frame_version > self.version {
                self.apply_committed(frame.operations())?;
                self.version = frame_version;
            }
        }

        Ok(())
    }

    pub(crate) fn materialize(
        &mut self,
        base: Arc<World>,
        intents: Vec<QueuedIntent>,
        validators: &[Validator],
    ) -> Result<(EpochPlan, bool), String> {
        if !validators.is_empty() {
            let plan = derive_epoch_world(base, intents, validators);
            if let Some(frame) = plan.frames().first() {
                self.apply_committed(frame.operations())?;
            }
            return Ok((plan, false));
        }
        self.derive_vm_epoch(base, intents).map(|plan| (plan, true))
    }

    pub fn materialize_mixed(
        &mut self,
        base: Arc<World>,
        intents: Vec<crate::queued_controller::ControllerIntent>,
        validators: &[Validator],
    ) -> MixedEpochPlan {
        enum MixedVmOutcome {
            QueuedAccepted {
                position: usize,
                entities: BTreeMap<TempEntity, EntityId>,
            },
            QueuedRejected(RejectedIntent),
            SemanticAccepted {
                position: usize,
                bindings: crate::transaction_ast::SemanticBindings,
            },
            SemanticRejected(RejectedSemanticIntent),
        }

        let mut predecessor_id = base.id();
        let mut predecessor_version = base.version();
        let mut accepted_operations = Vec::new();
        let mut outcomes = Vec::with_capacity(intents.len());
        let mut accepted_count = 0usize;

        let kernel = base.kernel().clone();

        for (position, intent) in intents.into_iter().enumerate() {
            if !validators.is_empty() {
                // Host validators path: validate against CandidateWorld BEFORE any VM or Materializer mutation
                match intent {
                    crate::queued_controller::ControllerIntent::Queued(queued) => {
                        if let Err(error) =
                            self.check_preconditions(predecessor_id, &queued.preconditions)
                        {
                            outcomes.push(MixedVmOutcome::QueuedRejected(RejectedIntent {
                                position,
                                error,
                            }));
                            continue;
                        }
                        let (operations, entities) = match resolve_operations_from(
                            self.workspace.next_entity(),
                            queued.namespace,
                            queued.operations,
                        ) {
                            Ok(resolved) => resolved,
                            Err(error) => {
                                outcomes.push(MixedVmOutcome::QueuedRejected(RejectedIntent {
                                    position,
                                    error,
                                }));
                                continue;
                            }
                        };
                        let candidate = match CandidateWorld::construct_from_state(
                            predecessor_id,
                            predecessor_version,
                            self.workspace.next_entity(),
                            base.operation_count,
                            &kernel,
                            operations.clone(),
                        ) {
                            Ok(c) => c,
                            Err(e) => {
                                outcomes.push(MixedVmOutcome::QueuedRejected(RejectedIntent {
                                    position,
                                    error: IntentRejection::Candidate(e),
                                }));
                                continue;
                            }
                        };
                        let mut val_err = None;
                        for validator in validators {
                            if let Err(e) = validator(&candidate) {
                                val_err = Some(e);
                                break;
                            }
                        }
                        if let Some(e) = val_err {
                            outcomes.push(MixedVmOutcome::QueuedRejected(RejectedIntent {
                                position,
                                error: IntentRejection::Validation(e),
                            }));
                            continue;
                        }

                        let program = match self.compile_operations(&operations) {
                            Ok(p) => p,
                            Err(e) => {
                                outcomes.push(MixedVmOutcome::QueuedRejected(RejectedIntent {
                                    position,
                                    error: IntentRejection::Validation(e),
                                }));
                                continue;
                            }
                        };

                        match self.workspace.execute(&program) {
                            ExecutionOutcome::Accepted => {
                                predecessor_version = predecessor_version.saturating_add(1);
                                predecessor_id = calculate_world_id(
                                    predecessor_id,
                                    predecessor_version,
                                    self.workspace.next_entity(),
                                    &operations,
                                );
                                accepted_operations.extend(operations);
                                accepted_count += 1;
                                outcomes
                                    .push(MixedVmOutcome::QueuedAccepted { position, entities });
                            }
                            ExecutionOutcome::Rejected(error) => {
                                outcomes.push(MixedVmOutcome::QueuedRejected(RejectedIntent {
                                    position,
                                    error: IntentRejection::Validation(format!("{error:?}")),
                                }));
                            }
                        }
                    }
                    crate::queued_controller::ControllerIntent::Semantic(semantic) => {
                        let intent_next_entity = self.workspace.next_entity();
                        let mut temps = std::collections::BTreeMap::new();
                        let mut current_entity = intent_next_entity;
                        for op in &semantic.ast.operations {
                            if let crate::transaction_ast::TransactionOp::Allocate { result } = op {
                                temps.insert(result.to_string(), current_entity);
                                current_entity += 1;
                            }
                        }

                        let mut synthesized_operations = Vec::new();
                        for op in &semantic.ast.operations {
                            use crate::transaction_ast::{AtomRef, TransactionOp};
                            match op {
                                TransactionOp::Allocate { result } => {
                                    synthesized_operations.push(Operation::AllocateEntity {
                                        entity: EntityId::new(*temps.get(result).unwrap()),
                                    });
                                }
                                TransactionOp::Define {
                                    slot,
                                    subject,
                                    predicate,
                                    object,
                                } => {
                                    let resolve = |r: &AtomRef| match r {
                                        AtomRef::Entity(e) => Atom::Entity(EntityId::new(e.0)),
                                        AtomRef::Literal(l) => {
                                            Atom::Literal(Literal::new(l.as_str()))
                                        }
                                        AtomRef::Symbol(s) => {
                                            Atom::Entity(EntityId::new(*temps.get(s).unwrap()))
                                        }
                                    };
                                    synthesized_operations.push(Operation::Define {
                                        slot: SlotId::new(slot.as_str()),
                                        fact: Fact {
                                            subject: resolve(subject),
                                            predicate: Predicate::new(predicate.as_str()),
                                            object: resolve(object),
                                        },
                                    });
                                }
                                TransactionOp::Forget { slot } => {
                                    synthesized_operations.push(Operation::Forget {
                                        slot: SlotId::new(slot.as_str()),
                                    });
                                }
                                _ => {}
                            }
                        }

                        let candidate = match CandidateWorld::construct_from_state(
                            predecessor_id,
                            predecessor_version,
                            intent_next_entity,
                            base.operation_count,
                            &kernel,
                            synthesized_operations.clone(),
                        ) {
                            Ok(c) => c,
                            Err(e) => {
                                outcomes.push(MixedVmOutcome::SemanticRejected(RejectedSemanticIntent { position, error: crate::queued_controller::SemanticTicketRejection::Validation(format!("candidate error: {e:?}")) }));
                                continue;
                            }
                        };

                        let mut val_err = None;
                        for validator in validators {
                            if let Err(e) = validator(&candidate) {
                                val_err = Some(e);
                                break;
                            }
                        }
                        if let Some(e) = val_err {
                            outcomes.push(MixedVmOutcome::SemanticRejected(RejectedSemanticIntent { position, error: crate::queued_controller::SemanticTicketRejection::Validation(e) }));
                            continue;
                        }

                        let operations_view: Vec<_> = semantic
                            .ast
                            .operations
                            .iter()
                            .map(|op| {
                                use crate::transaction_ast::{
                                    AtomRef, AtomRefView, TransactionOp, TransactionOpView,
                                };
                                match op {
                                    TransactionOp::Allocate { result } => {
                                        TransactionOpView::Allocate {
                                            result: result.as_str(),
                                        }
                                    }
                                    TransactionOp::ExpectWorld { expected } => {
                                        TransactionOpView::ExpectWorld {
                                            expected: *expected,
                                        }
                                    }
                                    TransactionOp::ExpectObject { slot, expected } => {
                                        TransactionOpView::ExpectObject {
                                            slot: slot.as_str(),
                                            expected: match expected {
                                                AtomRef::Entity(e) => AtomRefView::Entity(*e),
                                                AtomRef::Literal(l) => {
                                                    AtomRefView::Literal(l.as_str())
                                                }
                                                AtomRef::Symbol(s) => {
                                                    AtomRefView::Symbol(s.as_str())
                                                }
                                            },
                                        }
                                    }
                                    TransactionOp::Define {
                                        slot,
                                        subject,
                                        predicate,
                                        object,
                                    } => TransactionOpView::Define {
                                        slot: slot.as_str(),
                                        subject: match subject {
                                            AtomRef::Entity(e) => AtomRefView::Entity(*e),
                                            AtomRef::Literal(l) => AtomRefView::Literal(l.as_str()),
                                            AtomRef::Symbol(s) => AtomRefView::Symbol(s.as_str()),
                                        },
                                        predicate: predicate.as_str(),
                                        object: match object {
                                            AtomRef::Entity(e) => AtomRefView::Entity(*e),
                                            AtomRef::Literal(l) => AtomRefView::Literal(l.as_str()),
                                            AtomRef::Symbol(s) => AtomRefView::Symbol(s.as_str()),
                                        },
                                    },
                                    TransactionOp::Forget { slot } => TransactionOpView::Forget {
                                        slot: slot.as_str(),
                                    },
                                    TransactionOp::Reject => TransactionOpView::Reject,
                                }
                            })
                            .collect();

                        let view = crate::transaction_ast::TransactionView::borrowed(
                            semantic.ast.namespace,
                            &operations_view,
                        );

                        let mut trial = TrialVocabulary::new(self);
                        let (program, bindings) = match trial.compile_semantic_trial(&view) {
                            Ok(res) => res,
                            Err(e) => {
                                outcomes
                                    .push(MixedVmOutcome::SemanticRejected(RejectedSemanticIntent {
                                    position,
                                    error:
                                        crate::queued_controller::SemanticTicketRejection::Lowering(
                                            e,
                                        ),
                                }));
                                continue;
                            }
                        };

                        match trial.materializer.workspace.execute(&program) {
                            ExecutionOutcome::Accepted => {
                                trial.commit();
                                debug_assert_eq!(
                                    self.workspace.next_entity(),
                                    intent_next_entity + (temps.len() as u64),
                                    "VM next_entity after execution must match allocator advancement implied by operations"
                                );
                                predecessor_version = predecessor_version.saturating_add(1);
                                predecessor_id = calculate_world_id(
                                    predecessor_id,
                                    predecessor_version,
                                    self.workspace.next_entity(),
                                    &synthesized_operations,
                                );
                                accepted_operations.extend(synthesized_operations);
                                accepted_count += 1;
                                let semantic_bindings =
                                    crate::transaction_ast::SemanticBindings::new(
                                        bindings
                                            .into_iter()
                                            .map(|(k, v)| {
                                                (k, crate::transaction_ast::EntityId(v.value()))
                                            })
                                            .collect::<Vec<_>>(),
                                    );
                                outcomes.push(MixedVmOutcome::SemanticAccepted {
                                    position,
                                    bindings: semantic_bindings,
                                });
                            }
                            ExecutionOutcome::Rejected(e) => {
                                outcomes.push(MixedVmOutcome::SemanticRejected(RejectedSemanticIntent { position, error: crate::queued_controller::SemanticTicketRejection::Validation(format!("{e:?}")) }));
                            }
                        }
                    }
                }
            } else {
                // Fast path: zero-allocation trial execution directly on VM
                match intent {
                    crate::queued_controller::ControllerIntent::Queued(queued) => {
                        if let Err(error) =
                            self.check_preconditions(predecessor_id, &queued.preconditions)
                        {
                            outcomes.push(MixedVmOutcome::QueuedRejected(RejectedIntent {
                                position,
                                error,
                            }));
                            continue;
                        }
                        let (operations, entities) = match resolve_operations_from(
                            self.workspace.next_entity(),
                            queued.namespace,
                            queued.operations,
                        ) {
                            Ok(resolved) => resolved,
                            Err(error) => {
                                outcomes.push(MixedVmOutcome::QueuedRejected(RejectedIntent {
                                    position,
                                    error,
                                }));
                                continue;
                            }
                        };
                        let program = match self.compile_operations(&operations) {
                            Ok(p) => p,
                            Err(e) => {
                                outcomes.push(MixedVmOutcome::QueuedRejected(RejectedIntent {
                                    position,
                                    error: IntentRejection::Validation(e),
                                }));
                                continue;
                            }
                        };

                        match self.workspace.execute(&program) {
                            ExecutionOutcome::Accepted => {
                                predecessor_version = predecessor_version.saturating_add(1);
                                predecessor_id = calculate_world_id(
                                    predecessor_id,
                                    predecessor_version,
                                    self.workspace.next_entity(),
                                    &operations,
                                );
                                accepted_operations.extend(operations);
                                accepted_count += 1;
                                outcomes
                                    .push(MixedVmOutcome::QueuedAccepted { position, entities });
                            }
                            ExecutionOutcome::Rejected(error) => {
                                outcomes.push(MixedVmOutcome::QueuedRejected(RejectedIntent {
                                    position,
                                    error: IntentRejection::Validation(format!("{error:?}")),
                                }));
                            }
                        }
                    }
                    crate::queued_controller::ControllerIntent::Semantic(semantic) => {
                        let operations_view: Vec<_> = semantic
                            .ast
                            .operations
                            .iter()
                            .map(|op| {
                                use crate::transaction_ast::{
                                    AtomRef, AtomRefView, TransactionOp, TransactionOpView,
                                };
                                match op {
                                    TransactionOp::Allocate { result } => {
                                        TransactionOpView::Allocate {
                                            result: result.as_str(),
                                        }
                                    }
                                    TransactionOp::ExpectWorld { expected } => {
                                        TransactionOpView::ExpectWorld {
                                            expected: *expected,
                                        }
                                    }
                                    TransactionOp::ExpectObject { slot, expected } => {
                                        TransactionOpView::ExpectObject {
                                            slot: slot.as_str(),
                                            expected: match expected {
                                                AtomRef::Entity(e) => AtomRefView::Entity(*e),
                                                AtomRef::Literal(l) => {
                                                    AtomRefView::Literal(l.as_str())
                                                }
                                                AtomRef::Symbol(s) => {
                                                    AtomRefView::Symbol(s.as_str())
                                                }
                                            },
                                        }
                                    }
                                    TransactionOp::Define {
                                        slot,
                                        subject,
                                        predicate,
                                        object,
                                    } => TransactionOpView::Define {
                                        slot: slot.as_str(),
                                        subject: match subject {
                                            AtomRef::Entity(e) => AtomRefView::Entity(*e),
                                            AtomRef::Literal(l) => AtomRefView::Literal(l.as_str()),
                                            AtomRef::Symbol(s) => AtomRefView::Symbol(s.as_str()),
                                        },
                                        predicate: predicate.as_str(),
                                        object: match object {
                                            AtomRef::Entity(e) => AtomRefView::Entity(*e),
                                            AtomRef::Literal(l) => AtomRefView::Literal(l.as_str()),
                                            AtomRef::Symbol(s) => AtomRefView::Symbol(s.as_str()),
                                        },
                                    },
                                    TransactionOp::Forget { slot } => TransactionOpView::Forget {
                                        slot: slot.as_str(),
                                    },
                                    TransactionOp::Reject => TransactionOpView::Reject,
                                }
                            })
                            .collect();

                        let view = crate::transaction_ast::TransactionView::borrowed(
                            semantic.ast.namespace,
                            &operations_view,
                        );

                        let mut trial = TrialVocabulary::new(self);
                        let (program, bindings) = match trial.compile_semantic_trial(&view) {
                            Ok(res) => res,
                            Err(e) => {
                                outcomes
                                    .push(MixedVmOutcome::SemanticRejected(RejectedSemanticIntent {
                                    position,
                                    error:
                                        crate::queued_controller::SemanticTicketRejection::Lowering(
                                            e,
                                        ),
                                }));
                                continue;
                            }
                        };

                        let intent_next_entity = trial.materializer.workspace.next_entity();
                        match trial.materializer.workspace.execute(&program) {
                            ExecutionOutcome::Accepted => {
                                trial.commit();

                                let mut temps = std::collections::BTreeMap::new();
                                let mut current_entity = intent_next_entity;
                                for op in &semantic.ast.operations {
                                    if let crate::transaction_ast::TransactionOp::Allocate {
                                        result,
                                    } = op
                                    {
                                        temps.insert(result.to_string(), current_entity);
                                        current_entity += 1;
                                    }
                                }

                                let mut synthesized_operations = Vec::new();
                                for op in &semantic.ast.operations {
                                    use crate::transaction_ast::{AtomRef, TransactionOp};
                                    match op {
                                        TransactionOp::Allocate { result } => {
                                            synthesized_operations.push(
                                                Operation::AllocateEntity {
                                                    entity: EntityId::new(
                                                        *temps.get(result).unwrap(),
                                                    ),
                                                },
                                            );
                                        }
                                        TransactionOp::Define {
                                            slot,
                                            subject,
                                            predicate,
                                            object,
                                        } => {
                                            let resolve = |r: &AtomRef| match r {
                                                AtomRef::Entity(e) => {
                                                    Atom::Entity(EntityId::new(e.0))
                                                }
                                                AtomRef::Literal(l) => {
                                                    Atom::Literal(Literal::new(l.as_str()))
                                                }
                                                AtomRef::Symbol(s) => Atom::Entity(EntityId::new(
                                                    *temps.get(s).unwrap(),
                                                )),
                                            };
                                            synthesized_operations.push(Operation::Define {
                                                slot: SlotId::new(slot.as_str()),
                                                fact: Fact {
                                                    subject: resolve(subject),
                                                    predicate: Predicate::new(predicate.as_str()),
                                                    object: resolve(object),
                                                },
                                            });
                                        }
                                        TransactionOp::Forget { slot } => {
                                            synthesized_operations.push(Operation::Forget {
                                                slot: SlotId::new(slot.as_str()),
                                            });
                                        }
                                        _ => {}
                                    }
                                }

                                predecessor_version = predecessor_version.saturating_add(1);
                                predecessor_id = calculate_world_id(
                                    predecessor_id,
                                    predecessor_version,
                                    self.workspace.next_entity(),
                                    &synthesized_operations,
                                );
                                accepted_operations.extend(synthesized_operations);
                                accepted_count += 1;
                                let semantic_bindings =
                                    crate::transaction_ast::SemanticBindings::new(
                                        bindings
                                            .into_iter()
                                            .map(|(k, v)| {
                                                (k, crate::transaction_ast::EntityId(v.value()))
                                            })
                                            .collect::<Vec<_>>(),
                                    );
                                outcomes.push(MixedVmOutcome::SemanticAccepted {
                                    position,
                                    bindings: semantic_bindings,
                                });
                            }
                            ExecutionOutcome::Rejected(e) => {
                                outcomes.push(MixedVmOutcome::SemanticRejected(RejectedSemanticIntent { position, error: crate::queued_controller::SemanticTicketRejection::Validation(format!("{e:?}")) }));
                            }
                        }
                    }
                }
            }
        }

        if accepted_count == 0 {
            return MixedEpochPlan::new(
                base.clone(),
                base,
                outcomes
                    .into_iter()
                    .map(|o| match o {
                        MixedVmOutcome::QueuedRejected(r) => MixedEpochOutcome::QueuedRejected(r),
                        MixedVmOutcome::SemanticRejected(r) => {
                            MixedEpochOutcome::SemanticRejected(r)
                        }
                        _ => unreachable!(),
                    })
                    .collect(),
                Vec::new(),
            );
        }

        let (world, frame) = World::from_vm_epoch(
            base.clone(),
            accepted_operations,
            self.workspace.next_entity(),
            self.workspace.active_slot_count(),
            self.workspace.delta_count(),
        );

        let final_outcomes = outcomes
            .into_iter()
            .map(|outcome| match outcome {
                MixedVmOutcome::QueuedAccepted { position, entities } => {
                    MixedEpochOutcome::QueuedAccepted(AcceptedIntent {
                        position,
                        world: world.clone(),
                        frame: frame.clone(),
                        entities,
                    })
                }
                MixedVmOutcome::QueuedRejected(r) => MixedEpochOutcome::QueuedRejected(r),
                MixedVmOutcome::SemanticAccepted { position, bindings } => {
                    MixedEpochOutcome::SemanticAccepted(AcceptedSemanticIntent {
                        position,
                        world: world.clone(),
                        frame: frame.clone(),
                        bindings,
                    })
                }
                MixedVmOutcome::SemanticRejected(r) => MixedEpochOutcome::SemanticRejected(r),
            })
            .collect();

        MixedEpochPlan::new(base.clone(), world, final_outcomes, vec![frame])
    }

    fn derive_vm_epoch(
        &mut self,
        base: Arc<World>,
        intents: Vec<QueuedIntent>,
    ) -> Result<EpochPlan, String> {
        enum VmOutcome {
            Accepted {
                position: usize,
                entities: BTreeMap<TempEntity, EntityId>,
            },
            Rejected(RejectedIntent),
        }

        if self.workspace.next_entity() != base.next_entity() {
            return Err(format!(
                "VM allocator {}, published world allocator {}",
                self.workspace.next_entity(),
                base.next_entity()
            ));
        }

        let mut predecessor_id = base.id();
        let mut predecessor_version = base.version();
        let mut accepted_operations = Vec::new();
        let mut outcomes = Vec::with_capacity(intents.len());
        let mut accepted_count = 0usize;

        for (position, intent) in intents.into_iter().enumerate() {
            if let Err(error) = self.check_preconditions(predecessor_id, &intent.preconditions) {
                outcomes.push(VmOutcome::Rejected(RejectedIntent { position, error }));
                continue;
            }

            let (operations, entities) = match resolve_operations_from(
                self.workspace.next_entity(),
                intent.namespace,
                intent.operations,
            ) {
                Ok(resolved) => resolved,
                Err(error) => {
                    outcomes.push(VmOutcome::Rejected(RejectedIntent { position, error }));
                    continue;
                }
            };
            let program = self.compile_operations(&operations)?;
            match self.workspace.execute(&program) {
                ExecutionOutcome::Accepted => {
                    predecessor_version = predecessor_version.saturating_add(1);
                    predecessor_id = calculate_world_id(
                        predecessor_id,
                        predecessor_version,
                        self.workspace.next_entity(),
                        &operations,
                    );
                    accepted_operations.extend(operations);
                    accepted_count += 1;
                    outcomes.push(VmOutcome::Accepted { position, entities });
                }
                ExecutionOutcome::Rejected(error) => {
                    return Err(format!(
                        "compiled durable intent was rejected by the VM: {error:?}"
                    ));
                }
            }
        }

        if accepted_count == 0 {
            return Ok(EpochPlan {
                base: base.clone(),
                tail: base,
                outcomes: outcomes
                    .into_iter()
                    .map(|outcome| match outcome {
                        VmOutcome::Rejected(rejected) => EpochOutcome::Rejected(rejected),
                        VmOutcome::Accepted { .. } => unreachable!(),
                    })
                    .collect(),
                frames: Vec::new(),
            });
        }

        // The token workspace is the authoritative semantic state. Publish a
        // lightweight immutable world root over its accepted frontiers; the
        // legacy ForthDb query kernel is materialized lazily only if a reader
        // actually invokes the compatibility query surface.
        let (world, frame) = World::from_vm_epoch(
            base.clone(),
            accepted_operations,
            self.workspace.next_entity(),
            self.workspace.active_slot_count(),
            self.workspace.delta_count(),
        );
        let outcomes = outcomes
            .into_iter()
            .map(|outcome| match outcome {
                VmOutcome::Accepted { position, entities } => {
                    EpochOutcome::Accepted(AcceptedIntent {
                        position,
                        world: world.clone(),
                        frame: frame.clone(),
                        entities,
                    })
                }
                VmOutcome::Rejected(rejected) => EpochOutcome::Rejected(rejected),
            })
            .collect();
        Ok(EpochPlan {
            base,
            tail: world,
            outcomes,
            frames: vec![frame],
        })
    }

    fn check_preconditions(
        &self,
        predecessor_id: WorldId,
        preconditions: &[IntentPrecondition],
    ) -> Result<(), IntentRejection> {
        for precondition in preconditions {
            match precondition {
                IntentPrecondition::ExpectedWorld(expected) if *expected != predecessor_id => {
                    return Err(IntentRejection::WorldPrecondition {
                        expected: *expected,
                        actual: predecessor_id,
                    });
                }
                IntentPrecondition::ExpectedWorld(_) => {}
                IntentPrecondition::ExpectedSlot { slot, expected } => {
                    let actual = self.resolve_fact(slot);
                    if actual != *expected {
                        return Err(IntentRejection::SlotPrecondition {
                            slot: slot.clone(),
                            expected: expected.clone(),
                            actual,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn apply_committed(&mut self, operations: &[Operation]) -> Result<(), String> {
        let program = self.compile_operations(operations)?;
        match self.workspace.execute(&program) {
            ExecutionOutcome::Accepted => Ok(()),
            ExecutionOutcome::Rejected(error) => Err(format!(
                "validated committed operations were rejected by the VM: {error:?}"
            )),
        }
    }

    fn compile_operations(&mut self, operations: &[Operation]) -> Result<IntentProgram, String> {
        let mut instructions = Vec::with_capacity(operations.len().saturating_mul(4));
        for operation in operations {
            match operation {
                Operation::AllocateEntity { entity } => {
                    if entity.value() >= VM_LITERAL_BASE {
                        return Err("entity allocator exhausted the VM atom namespace".to_owned());
                    }
                    instructions.push(Instruction::allocate_discard());
                }
                Operation::Define { slot, fact } => {
                    let slot = self.slot_token(slot);
                    let subject = self.atom_cell(&fact.subject)?;
                    let predicate = self.predicate_cell(&fact.predicate);
                    let object = self.atom_cell(&fact.object)?;
                    instructions.extend([
                        Instruction::push(subject),
                        Instruction::push(predicate),
                        Instruction::push(object),
                        Instruction::define(slot),
                    ]);
                }
                Operation::Forget { slot } => {
                    let slot = self.slot_token(slot);
                    instructions.push(Instruction::forget(slot));
                }
            }
        }
        Ok(IntentProgram::new(0, instructions))
    }

    fn slot_token(&mut self, slot: &SlotId) -> SlotToken {
        if let Some(token) = self.slots.get(slot) {
            return *token;
        }
        if let Some(token) = self
            .mapped_base
            .as_ref()
            .and_then(|base| base.slot_token(slot.as_str()))
        {
            return SlotToken(token);
        }
        let base_count = self
            .mapped_base
            .as_ref()
            .map_or(0, |base| base.slot_count());
        let token = SlotToken((base_count + self.slots.len()) as u32);
        self.slots.insert(slot.clone(), token);
        self.workspace
            .ensure_slot_count(base_count + self.slots.len());
        token
    }

    fn predicate_cell(&mut self, predicate: &Predicate) -> Cell {
        if let Some(cell) = self.predicates.get(predicate) {
            return *cell;
        }
        if let Some(token) = self
            .mapped_base
            .as_ref()
            .and_then(|base| base.predicate_token(predicate.as_str()))
        {
            return Cell(u64::from(token));
        }
        let base_count = self
            .mapped_base
            .as_ref()
            .map_or(0, |base| base.predicate_count());
        let cell = Cell((base_count + self.predicates.len()) as u64);
        self.predicates.insert(predicate.clone(), cell);
        self.predicate_values.push(predicate.clone());
        cell
    }

    fn atom_cell(&mut self, atom: &Atom) -> Result<Cell, String> {
        match atom {
            Atom::Entity(entity) if entity.value() < VM_LITERAL_BASE => Ok(Cell(entity.value())),
            Atom::Entity(_) => {
                Err("entity identifier overlaps the VM literal namespace".to_owned())
            }
            Atom::Literal(literal) => {
                if let Some(cell) = self.literals.get(literal) {
                    return Ok(*cell);
                }
                if let Some(token) = self
                    .mapped_base
                    .as_ref()
                    .and_then(|base| base.literal_token(literal.as_str()))
                {
                    return Ok(Cell(VM_LITERAL_BASE + u64::from(token)));
                }
                let base_count = self
                    .mapped_base
                    .as_ref()
                    .map_or(0, |base| base.literal_count());
                let offset = (base_count + self.literal_values.len()) as u64;
                let value = VM_LITERAL_BASE
                    .checked_add(offset)
                    .ok_or_else(|| "VM literal token overflow".to_owned())?;
                let cell = Cell(value);
                self.literals.insert(literal.clone(), cell);
                self.literal_values.push(literal.clone());
                Ok(cell)
            }
        }
    }

    fn resolve_fact(&self, slot: &SlotId) -> Option<Fact> {
        let token = self.slots.get(slot).copied().or_else(|| {
            self.mapped_base
                .as_ref()
                .and_then(|base| base.slot_token(slot.as_str()))
                .map(SlotToken)
        })?;
        let (subject, predicate, object) = self.workspace.resolve_fact_cells(token)?;
        Some(Fact::new(
            self.atom_from_cell(subject)?,
            self.predicate_from_cell(predicate)?,
            self.atom_from_cell(object)?,
        ))
    }

    fn atom_from_cell(&self, cell: Cell) -> Option<Atom> {
        if cell.0 < VM_LITERAL_BASE {
            Some(Atom::Entity(EntityId::new(cell.0)))
        } else {
            let token = (cell.0 - VM_LITERAL_BASE) as usize;
            if let Some(base) = self.mapped_base.as_ref() {
                if token < base.literal_count() {
                    return base.literal_value(token).map(Atom::Literal);
                }
                return self
                    .literal_values
                    .get(token - base.literal_count())
                    .cloned()
                    .map(Atom::Literal);
            }
            self.literal_values.get(token).cloned().map(Atom::Literal)
        }
    }

    fn predicate_from_cell(&self, cell: Cell) -> Option<Predicate> {
        let token = cell.0 as usize;
        if let Some(base) = self.mapped_base.as_ref() {
            if token < base.predicate_count() {
                return base.predicate_value(token);
            }
            return self
                .predicate_values
                .get(token - base.predicate_count())
                .cloned();
        }
        self.predicate_values.get(token).cloned()
    }
}

pub(crate) struct TrialVocabulary<'a> {
    materializer: &'a mut VmEpochMaterializer,
    slots: std::collections::BTreeMap<SlotId, SlotToken>,
    predicates: std::collections::BTreeMap<Predicate, Cell>,
    literals: std::collections::BTreeMap<Literal, Cell>,
}

impl<'a> TrialVocabulary<'a> {
    fn new(materializer: &'a mut VmEpochMaterializer) -> Self {
        Self {
            materializer,
            slots: std::collections::BTreeMap::new(),
            predicates: std::collections::BTreeMap::new(),
            literals: std::collections::BTreeMap::new(),
        }
    }

    fn commit(self) {
        for (k, v) in self.slots {
            self.materializer.slots.insert(k, v);
        }
        for (k, v) in self.predicates {
            self.materializer.predicates.insert(k.clone(), v);
            self.materializer.predicate_values.push(k);
        }
        for (k, v) in self.literals {
            self.materializer.literals.insert(k.clone(), v);
            self.materializer.literal_values.push(k);
        }
    }

    fn slot_token(&mut self, slot_str: &str) -> SlotToken {
        let slot = SlotId::new(slot_str);
        if let Some(token) = self.slots.get(&slot) {
            return *token;
        }
        if let Some(token) = self.materializer.slots.get(&slot) {
            return *token;
        }
        if let Some(token) = self
            .materializer
            .mapped_base
            .as_ref()
            .and_then(|base| base.slot_token(slot.as_str()))
        {
            return SlotToken(token);
        }
        let base_count = self
            .materializer
            .mapped_base
            .as_ref()
            .map_or(0, |base| base.slot_count());
        let total_slots = base_count + self.materializer.slots.len() + self.slots.len();
        let token = SlotToken(total_slots as u32);
        self.slots.insert(slot, token);
        self.materializer
            .workspace
            .ensure_slot_count(total_slots + 1);
        token
    }

    fn predicate_cell(&mut self, pred_str: &str) -> Cell {
        let predicate = Predicate::new(pred_str);
        if let Some(cell) = self.predicates.get(&predicate) {
            return *cell;
        }
        if let Some(cell) = self.materializer.predicates.get(&predicate) {
            return *cell;
        }
        if let Some(token) = self
            .materializer
            .mapped_base
            .as_ref()
            .and_then(|base| base.predicate_token(predicate.as_str()))
        {
            return Cell(u64::from(token));
        }
        let base_count = self
            .materializer
            .mapped_base
            .as_ref()
            .map_or(0, |base| base.predicate_count());
        let cell =
            Cell((base_count + self.materializer.predicates.len() + self.predicates.len()) as u64);
        self.predicates.insert(predicate, cell);
        cell
    }

    fn literal_cell(&mut self, lit_str: &str) -> Cell {
        let literal = Literal::new(lit_str);
        if let Some(cell) = self.literals.get(&literal) {
            return *cell;
        }
        if let Some(cell) = self.materializer.literals.get(&literal) {
            return *cell;
        }
        if let Some(token) = self
            .materializer
            .mapped_base
            .as_ref()
            .and_then(|base| base.literal_token(literal.as_str()))
        {
            return Cell(VM_LITERAL_BASE + u64::from(token));
        }
        let base_count = self
            .materializer
            .mapped_base
            .as_ref()
            .map_or(0, |base| base.literal_count());
        let offset =
            (base_count + self.materializer.literal_values.len() + self.literals.len()) as u64;
        let value = VM_LITERAL_BASE
            .checked_add(offset)
            .expect("VM literal token overflow");
        let cell = Cell(value);
        self.literals.insert(literal, cell);
        cell
    }

    pub(crate) fn compile_semantic_trial(
        &mut self,
        view: &crate::transaction_ast::TransactionView<'_>,
    ) -> Result<
        (
            crate::stack_vm::IntentProgram,
            std::collections::BTreeMap<String, EntityId>,
        ),
        String,
    > {
        use crate::stack_vm::Instruction;
        use crate::transaction_ast::{AtomRefView, TransactionOpView};

        let mut bindings = std::collections::BTreeMap::new();
        let mut instructions = Vec::with_capacity(view.operations.len() * 4);
        let mut local_entity_count = 0;

        for op in view.operations {
            match op {
                TransactionOpView::Allocate { result } => {
                    if bindings.contains_key(*result) {
                        return Err(format!("duplicate allocation for symbol '{result}'"));
                    }
                    let entity_id = self.materializer.workspace.next_entity() + local_entity_count;
                    if entity_id >= VM_LITERAL_BASE {
                        return Err("entity allocator exhausted the VM atom namespace".to_owned());
                    }
                    local_entity_count += 1;
                    let entity = EntityId::new(entity_id);
                    bindings.insert((*result).to_owned(), entity);
                    instructions.push(Instruction::allocate_discard());
                }
                TransactionOpView::ExpectWorld { .. } => {}
                TransactionOpView::ExpectObject { slot, expected } => {
                    let slot_token = self.slot_token(slot);
                    let expected_cell = match expected {
                        AtomRefView::Entity(e) => Cell(e.0),
                        AtomRefView::Literal(l) => self.literal_cell(l),
                        AtomRefView::Symbol(s) => Cell(
                            bindings
                                .get(*s)
                                .copied()
                                .ok_or_else(|| format!("use of undefined symbol '{s}'"))?
                                .value(),
                        ),
                    };
                    instructions.push(Instruction::expect_object(slot_token, expected_cell));
                }
                TransactionOpView::Define {
                    slot,
                    subject,
                    predicate,
                    object,
                } => {
                    let slot_token = self.slot_token(slot);
                    let sub_cell = match subject {
                        AtomRefView::Entity(e) => Cell(e.0),
                        AtomRefView::Literal(l) => self.literal_cell(l),
                        AtomRefView::Symbol(s) => Cell(
                            bindings
                                .get(*s)
                                .copied()
                                .ok_or_else(|| format!("use of undefined symbol '{s}'"))?
                                .value(),
                        ),
                    };
                    let pred_cell = self.predicate_cell(predicate);
                    let obj_cell = match object {
                        AtomRefView::Entity(e) => Cell(e.0),
                        AtomRefView::Literal(l) => self.literal_cell(l),
                        AtomRefView::Symbol(s) => Cell(
                            bindings
                                .get(*s)
                                .copied()
                                .ok_or_else(|| format!("use of undefined symbol '{s}'"))?
                                .value(),
                        ),
                    };
                    instructions.extend([
                        Instruction::push(sub_cell),
                        Instruction::push(pred_cell),
                        Instruction::push(obj_cell),
                        Instruction::define(slot_token),
                    ]);
                }
                TransactionOpView::Forget { slot } => {
                    let slot_token = self.slot_token(slot);
                    instructions.push(Instruction::forget(slot_token));
                }
                TransactionOpView::Reject => {
                    instructions.push(Instruction::reject());
                }
            }
        }

        Ok((
            crate::stack_vm::IntentProgram::new(0, instructions),
            bindings,
        ))
    }
}

/// Construct the accepted private world chain in ingress order.
///
/// Preconditions and validators are evaluated against the predecessor assigned
/// to each intent. A rejected intent consumes neither a version nor an entity
/// identifier, and the next intent continues from the preceding accepted world.
pub fn derive_epoch(
    base: Arc<World>,
    intents: Vec<QueuedIntent>,
    validators: &[Validator],
) -> EpochPlan {
    let mut predecessor = base.clone();
    let mut outcomes = Vec::with_capacity(intents.len());
    let mut frames = Vec::with_capacity(intents.len());

    for (position, intent) in intents.into_iter().enumerate() {
        match derive_intent(predecessor.clone(), intent, validators) {
            Ok((world, frame, entities)) => {
                predecessor = world.clone();
                frames.push(frame.clone());
                outcomes.push(EpochOutcome::Accepted(AcceptedIntent {
                    position,
                    world,
                    frame,
                    entities,
                }));
            }
            Err(error) => outcomes.push(EpochOutcome::Rejected(RejectedIntent { position, error })),
        }
    }

    EpochPlan {
        base,
        tail: predecessor,
        outcomes,
        frames,
    }
}

/// Construct one immutable successor world for an ordered admission epoch.
///
/// Intents are still evaluated in ingress order against a private evolving
/// candidate so preconditions, allocation, and independent rejection retain
/// their established meaning. Accepted operations are then collapsed into one
/// canonical frame rooted at the published predecessor. Every accepted ticket
/// observes the same epoch world; no intermediate private candidate is
/// externally publishable.
pub fn derive_epoch_world(
    base: Arc<World>,
    intents: Vec<QueuedIntent>,
    validators: &[Validator],
) -> EpochPlan {
    enum WorkspaceOutcome {
        Accepted {
            position: usize,
            entities: BTreeMap<TempEntity, EntityId>,
        },
        Rejected(RejectedIntent),
    }

    // The workspace owns the only evolving semantic state for the epoch. Each
    // intent gets a trial candidate so rejection remains isolated, but an
    // accepted candidate is moved back into the workspace rather than being
    // wrapped in a World, linked into history, and cloned again by the next
    // intent. The synthetic id/version chain retains predecessor-relative
    // precondition and validator behavior until the epoch is collapsed.
    let mut predecessor_id = base.id;
    let mut predecessor_version = base.version;
    let mut next_entity = base.next_entity;
    let mut operation_count = base.operation_count;
    let mut kernel = base.kernel().clone();
    let mut operations = Vec::new();
    let mut accepted_count = 0usize;
    let mut workspace_outcomes = Vec::with_capacity(intents.len());

    for (position, intent) in intents.into_iter().enumerate() {
        let result = (|| {
            check_workspace_preconditions(predecessor_id, &kernel, &intent.preconditions)?;
            let (intent_operations, entities) =
                resolve_operations_from(next_entity, intent.namespace, intent.operations)?;
            let candidate = CandidateWorld::construct_from_state(
                predecessor_id,
                predecessor_version,
                next_entity,
                operation_count,
                &kernel,
                intent_operations,
            )
            .map_err(IntentRejection::Candidate)?;
            for validator in validators {
                validator(&candidate).map_err(IntentRejection::Validation)?;
            }
            Ok((candidate, entities))
        })();

        match result {
            Ok((candidate, entities)) => {
                let accepted_operations;
                (
                    predecessor_id,
                    predecessor_version,
                    next_entity,
                    operation_count,
                    accepted_operations,
                    kernel,
                ) = candidate.into_materialized_state();
                operations.extend(accepted_operations.iter().cloned());
                accepted_count += 1;
                workspace_outcomes.push(WorkspaceOutcome::Accepted { position, entities });
            }
            Err(error) => workspace_outcomes.push(WorkspaceOutcome::Rejected(RejectedIntent {
                position,
                error,
            })),
        }
    }

    if accepted_count == 0 {
        return EpochPlan {
            base: base.clone(),
            tail: base,
            outcomes: workspace_outcomes
                .into_iter()
                .map(|outcome| match outcome {
                    WorkspaceOutcome::Rejected(rejected) => EpochOutcome::Rejected(rejected),
                    WorkspaceOutcome::Accepted { .. } => unreachable!(),
                })
                .collect(),
            frames: Vec::new(),
        };
    }

    // Materialization already updated the kernel and indexes incrementally.
    // Build only the externally visible epoch candidate and world here; do not
    // replay all accepted operations against the base a second time.
    let version = base.version + 1;
    let id = calculate_world_id(base.id, version, next_entity, &operations);
    let candidate = CandidateWorld {
        base_world: base.id,
        base_version: base.version,
        id,
        version,
        next_entity,
        base_operation_count: base.operation_count,
        operations: Arc::from(operations),
        kernel,
    };
    let frame = candidate.commit_frame();
    let world = Arc::new(candidate.into_world(frame.clone(), base.history.clone()));
    let outcomes = workspace_outcomes
        .into_iter()
        .map(|outcome| match outcome {
            WorkspaceOutcome::Accepted { position, entities } => {
                EpochOutcome::Accepted(AcceptedIntent {
                    position,
                    world: world.clone(),
                    frame: frame.clone(),
                    entities,
                })
            }
            WorkspaceOutcome::Rejected(rejected) => EpochOutcome::Rejected(rejected),
        })
        .collect();

    EpochPlan {
        base,
        tail: world,
        outcomes,
        frames: vec![frame],
    }
}

fn check_workspace_preconditions(
    predecessor_id: WorldId,
    kernel: &ForthDb,
    preconditions: &[IntentPrecondition],
) -> Result<(), IntentRejection> {
    for precondition in preconditions {
        match precondition {
            IntentPrecondition::ExpectedWorld(expected) if *expected != predecessor_id => {
                return Err(IntentRejection::WorldPrecondition {
                    expected: *expected,
                    actual: predecessor_id,
                });
            }
            IntentPrecondition::ExpectedWorld(_) => {}
            IntentPrecondition::ExpectedSlot { slot, expected } => {
                let actual = kernel.resolve(slot).cloned();
                if actual != *expected {
                    return Err(IntentRejection::SlotPrecondition {
                        slot: slot.clone(),
                        expected: expected.clone(),
                        actual,
                    });
                }
            }
        }
    }
    Ok(())
}

fn derive_intent(
    predecessor: Arc<World>,
    intent: QueuedIntent,
    validators: &[Validator],
) -> Result<(Arc<World>, Arc<CommitFrame>, BTreeMap<TempEntity, EntityId>), IntentRejection> {
    check_preconditions(&predecessor, &intent.preconditions)?;
    let (operations, entities) =
        resolve_operations(&predecessor, intent.namespace, intent.operations)?;
    let candidate = CandidateWorld::construct(predecessor.as_ref(), operations)
        .map_err(IntentRejection::Candidate)?;
    for validator in validators {
        validator(&candidate).map_err(IntentRejection::Validation)?;
    }
    let frame = candidate.commit_frame();
    let world = Arc::new(candidate.into_world(frame.clone(), predecessor.history.clone()));
    Ok((world, frame, entities))
}

fn check_preconditions(
    predecessor: &World,
    preconditions: &[IntentPrecondition],
) -> Result<(), IntentRejection> {
    for precondition in preconditions {
        match precondition {
            IntentPrecondition::ExpectedWorld(expected) if *expected != predecessor.id() => {
                return Err(IntentRejection::WorldPrecondition {
                    expected: *expected,
                    actual: predecessor.id(),
                });
            }
            IntentPrecondition::ExpectedWorld(_) => {}
            IntentPrecondition::ExpectedSlot { slot, expected } => {
                let actual = predecessor.resolve(slot).cloned();
                if actual != *expected {
                    return Err(IntentRejection::SlotPrecondition {
                        slot: slot.clone(),
                        expected: expected.clone(),
                        actual,
                    });
                }
            }
        }
    }
    Ok(())
}

fn resolve_operations(
    predecessor: &World,
    namespace: u64,
    intent_operations: Vec<IntentOperation>,
) -> Result<(Vec<Operation>, BTreeMap<TempEntity, EntityId>), IntentRejection> {
    resolve_operations_from(predecessor.next_entity(), namespace, intent_operations)
}

fn resolve_operations_from(
    mut next_entity: u64,
    namespace: u64,
    intent_operations: Vec<IntentOperation>,
) -> Result<(Vec<Operation>, BTreeMap<TempEntity, EntityId>), IntentRejection> {
    let mut entities = BTreeMap::new();
    let mut operations = Vec::with_capacity(intent_operations.len());

    for operation in intent_operations {
        match operation {
            IntentOperation::AllocateEntity { temporary } => {
                if temporary.namespace != namespace {
                    return Err(IntentRejection::UnknownTemporaryEntity(temporary));
                }
                let entity = EntityId::new(next_entity);
                next_entity = next_entity
                    .checked_add(1)
                    .expect("world entity allocator overflow");
                entities.insert(temporary, entity);
                operations.push(Operation::AllocateEntity { entity });
            }
            IntentOperation::Define { slot, fact } => operations.push(Operation::Define {
                slot,
                fact: resolve_fact(fact, namespace, &entities)?,
            }),
            IntentOperation::Forget { slot } => operations.push(Operation::Forget { slot }),
        }
    }

    Ok((operations, entities))
}

fn resolve_fact(
    fact: IntentFact,
    namespace: u64,
    entities: &BTreeMap<TempEntity, EntityId>,
) -> Result<Fact, IntentRejection> {
    Ok(Fact::new(
        resolve_atom(fact.subject, namespace, entities)?,
        fact.predicate,
        resolve_atom(fact.object, namespace, entities)?,
    ))
}

fn resolve_atom(
    atom: IntentAtom,
    namespace: u64,
    entities: &BTreeMap<TempEntity, EntityId>,
) -> Result<Atom, IntentRejection> {
    match atom {
        IntentAtom::Entity(entity) => Ok(Atom::Entity(entity)),
        IntentAtom::Temporary(temporary) => {
            if temporary.namespace != namespace {
                return Err(IntentRejection::UnknownTemporaryEntity(temporary));
            }
            entities
                .get(&temporary)
                .copied()
                .map(Atom::Entity)
                .ok_or(IntentRejection::UnknownTemporaryEntity(temporary))
        }
        IntentAtom::Literal(literal) => Ok(Atom::Literal(literal)),
    }
}

/// Stage 6A's in-memory publication control. It appends every accepted frame to
/// the infallible store, then changes the global reader head exactly once.
impl Database<MemoryCommitStore> {
    pub fn commit_mixed_epoch(
        &self,
        intents: Vec<crate::queued_controller::ControllerIntent>,
        materializer: &mut VmEpochMaterializer,
    ) -> MixedEpochPlan {
        let _commit_guard = self
            .commit_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let base = self.snapshot();
        let validators = self
            .validators
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        if let Err(e) = materializer.synchronize_to(&base) {
            panic!("fatal materializer synchronization failure: {e}");
        }

        let plan = materializer.materialize_mixed(base, intents, &validators);

        if !plan.is_empty() {
            let mut store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for frame in plan.frames() {
                match store.append(frame.clone()) {
                    Ok(()) => {}
                    Err(never) => match never {},
                }
            }
            drop(store);
            *self
                .current
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = plan.tail();
        }

        materializer.version = plan.tail().version();

        plan
    }

    pub fn commit_queued_epoch(&self, intents: Vec<QueuedIntent>) -> EpochPlan {
        let _commit_guard = self
            .commit_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let base = self.snapshot();
        let validators = self
            .validators
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let plan = derive_epoch(base, intents, &validators);

        if !plan.is_empty() {
            let mut store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for frame in plan.frames() {
                match store.append(frame.clone()) {
                    Ok(()) => {}
                    Err(never) => match never {},
                }
            }
            drop(store);
            *self
                .current
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = plan.tail();
        }

        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_store::FileCommitStore;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn state_fact(entity: EntityId, value: &str) -> Fact {
        Fact::new(
            Atom::Entity(entity),
            Predicate::new("state"),
            Atom::Literal(Literal::new(value)),
        )
    }

    fn intent_state_fact(entity: impl Into<IntentAtom>, value: &str) -> IntentFact {
        IntentFact::new(entity, Predicate::new("state"), Literal::new(value))
    }

    fn replay_frame(database: &Database<MemoryCommitStore>, frame: &CommitFrame) -> Arc<World> {
        let mut transaction = database.begin();
        for operation in frame.operations() {
            match operation {
                Operation::AllocateEntity { entity } => {
                    assert_eq!(transaction.entity(), *entity);
                }
                Operation::Define { slot, fact } => {
                    transaction.define(slot.clone(), fact.clone());
                }
                Operation::Forget { slot } => transaction.forget(slot.clone()),
            }
        }
        database
            .commit(transaction)
            .expect("oracle commit succeeds")
    }

    #[test]
    fn temporary_entities_are_scoped_and_resolved_from_each_predecessor() {
        let mut first = QueuedIntent::new();
        let first_temp = first.entity();
        first.define(
            SlotId::new("first/state"),
            intent_state_fact(first_temp, "ready"),
        );

        let mut second = QueuedIntent::new();
        let second_temp = second.entity();
        second.define(
            SlotId::new("second/state"),
            intent_state_fact(second_temp, "ready"),
        );

        assert_eq!(first_temp.index(), second_temp.index());
        assert_ne!(first_temp, second_temp);
        let plan = derive_epoch(Arc::new(World::genesis()), vec![first, second], &[]);
        let first = plan.outcomes()[0].accepted().expect("first accepted");
        let second = plan.outcomes()[1].accepted().expect("second accepted");
        assert_eq!(first.entity(first_temp), Some(EntityId::new(1)));
        assert_eq!(second.entity(second_temp), Some(EntityId::new(2)));
        assert_eq!(plan.tail().next_entity(), 3);
    }

    #[test]
    fn a_foreign_temporary_handle_cannot_alias_a_local_handle() {
        let mut owner = QueuedIntent::new();
        let foreign = owner.entity();

        let mut target = QueuedIntent::new();
        let local = target.entity();
        assert_eq!(foreign.index(), local.index());
        target.define(
            SlotId::new("target/state"),
            intent_state_fact(foreign, "invalid"),
        );

        let plan = derive_epoch(Arc::new(World::genesis()), vec![target], &[]);
        assert!(matches!(
            plan.outcomes()[0].rejected().expect("rejected").error(),
            IntentRejection::UnknownTemporaryEntity(entity) if *entity == foreign
        ));
        assert_eq!(plan.tail().version(), 0);
        assert_eq!(plan.tail().next_entity(), 1);
    }

    #[test]
    fn a_rejected_intent_consumes_neither_version_nor_allocator_state() {
        let mut accepted_first = QueuedIntent::new();
        let first_entity = accepted_first.entity();
        accepted_first.define(
            SlotId::new("first/state"),
            intent_state_fact(first_entity, "ready"),
        );

        let mut rejected = QueuedIntent::new();
        rejected.expect_world(WorldId::new(7));
        let rejected_entity = rejected.entity();
        rejected.define(
            SlotId::new("rejected/state"),
            intent_state_fact(rejected_entity, "never"),
        );

        let mut accepted_last = QueuedIntent::new();
        let last_entity = accepted_last.entity();
        accepted_last.define(
            SlotId::new("last/state"),
            intent_state_fact(last_entity, "ready"),
        );

        let plan = derive_epoch(
            Arc::new(World::genesis()),
            vec![accepted_first, rejected, accepted_last],
            &[],
        );
        assert_eq!(plan.accepted_count(), 2);
        assert_eq!(plan.rejected_count(), 1);
        assert_eq!(plan.tail().version(), 2);
        assert_eq!(plan.tail().next_entity(), 3);
        let last = plan.outcomes()[2].accepted().expect("last accepted");
        assert_eq!(last.entity(last_entity), Some(EntityId::new(2)));
        assert!(
            plan.tail()
                .resolve(&SlotId::new("rejected/state"))
                .is_none()
        );
    }

    #[test]
    fn slot_preconditions_observe_the_assigned_private_predecessor() {
        let slot = SlotId::new("service/state");
        let entity = EntityId::new(1);
        let initial = state_fact(entity, "one");
        let updated = state_fact(entity, "two");
        let database = Database::new(MemoryCommitStore::new()).expect("genesis valid");
        let mut setup = database.begin();
        assert_eq!(setup.entity(), entity);
        setup.define(slot.clone(), initial.clone());
        let base = database.commit(setup).expect("setup commits");

        let mut first = QueuedIntent::new();
        first.expect_value(slot.clone(), initial.clone());
        first.define_fact(slot.clone(), updated.clone());

        let mut stale = QueuedIntent::new();
        stale.expect_value(slot.clone(), initial);
        stale.define_fact(
            SlotId::new("should/not/exist"),
            Fact::new(
                Atom::Literal(Literal::new("x")),
                Predicate::new("value"),
                Atom::Literal(Literal::new("y")),
            ),
        );

        let mut current = QueuedIntent::new();
        current.expect_value(slot.clone(), updated.clone());
        current.define_fact(
            SlotId::new("should/exist"),
            Fact::new(
                Atom::Literal(Literal::new("x")),
                Predicate::new("value"),
                Atom::Literal(Literal::new("z")),
            ),
        );

        let plan = derive_epoch(base, vec![first, stale, current], &[]);
        assert!(plan.outcomes()[0].accepted().is_some());
        assert!(matches!(
            plan.outcomes()[1].rejected().expect("rejected").error(),
            IntentRejection::SlotPrecondition { .. }
        ));
        assert!(plan.outcomes()[2].accepted().is_some());
        assert_eq!(plan.tail().resolve(&slot), Some(&updated));
        assert!(
            plan.tail()
                .resolve(&SlotId::new("should/not/exist"))
                .is_none()
        );
        assert!(plan.tail().resolve(&SlotId::new("should/exist")).is_some());
    }

    #[test]
    fn validators_reject_only_the_assigned_intent() {
        let required = SlotId::new("approval");
        let validator_slot = required.clone();
        let validator: Validator = Arc::new(move |candidate| {
            candidate
                .resolve(&validator_slot)
                .map(|_| ())
                .ok_or_else(|| "approval required".to_owned())
        });

        let mut rejected = QueuedIntent::new();
        rejected.define_fact(
            SlotId::new("work/one"),
            Fact::new(
                Atom::Literal(Literal::new("one")),
                Predicate::new("state"),
                Atom::Literal(Literal::new("ready")),
            ),
        );

        let mut accepted = QueuedIntent::new();
        accepted.define_fact(
            required,
            Fact::new(
                Atom::Literal(Literal::new("release")),
                Predicate::new("approved_by"),
                Atom::Literal(Literal::new("operator")),
            ),
        );

        let plan = derive_epoch(
            Arc::new(World::genesis()),
            vec![rejected, accepted],
            &[validator],
        );
        assert!(matches!(
            plan.outcomes()[0].rejected().expect("rejected").error(),
            IntentRejection::Validation(_)
        ));
        assert!(plan.outcomes()[1].accepted().is_some());
        assert_eq!(plan.tail().version(), 1);
    }

    #[test]
    fn epoch_workspace_preserves_ordered_semantics_and_publishes_one_world() {
        let slot = SlotId::new("workspace/state");
        let initial = state_fact(EntityId::new(1), "initial");
        let updated = state_fact(EntityId::new(1), "updated");
        let database = Database::new(MemoryCommitStore::new()).expect("genesis valid");
        let mut setup = database.begin();
        assert_eq!(setup.entity(), EntityId::new(1));
        setup.define(slot.clone(), initial.clone());
        let base = database.commit(setup).expect("setup commits");

        let mut first = QueuedIntent::new();
        first.expect_value(slot.clone(), initial.clone());
        first.define_fact(slot.clone(), updated.clone());

        let mut rejected = QueuedIntent::new();
        rejected.expect_value(slot.clone(), initial);
        let rejected_entity = rejected.entity();
        rejected.define(
            SlotId::new("workspace/rejected"),
            intent_state_fact(rejected_entity, "never"),
        );

        let mut last = QueuedIntent::new();
        last.expect_value(slot.clone(), updated.clone());
        let last_entity = last.entity();
        last.define(
            SlotId::new("workspace/accepted"),
            intent_state_fact(last_entity, "ready"),
        );

        let mut validator_rejected = QueuedIntent::new();
        let validator_rejected_entity = validator_rejected.entity();
        validator_rejected.define(
            SlotId::new("workspace/validator-rejected"),
            intent_state_fact(validator_rejected_entity, "never"),
        );
        let validator: Validator = Arc::new(|candidate| {
            if candidate
                .resolve(&SlotId::new("workspace/validator-rejected"))
                .is_some()
            {
                Err("deliberate rejection".to_owned())
            } else {
                Ok(())
            }
        });

        let plan = derive_epoch_world(
            base.clone(),
            vec![first, rejected, validator_rejected, last],
            &[validator],
        );
        assert!(plan.outcomes()[0].accepted().is_some());
        assert!(matches!(
            plan.outcomes()[1].rejected().expect("rejected").error(),
            IntentRejection::SlotPrecondition { .. }
        ));
        assert!(matches!(
            plan.outcomes()[2].rejected().expect("rejected").error(),
            IntentRejection::Validation(_)
        ));
        let accepted = plan.outcomes()[3].accepted().expect("last accepted");
        assert_eq!(accepted.entity(last_entity), Some(EntityId::new(2)));
        assert_eq!(accepted.world().id(), plan.tail().id());
        assert_eq!(plan.frames().len(), 1);
        assert_eq!(plan.tail().version(), base.version() + 1);
        assert_eq!(plan.frames()[0].parent_world(), base.id());
        assert_eq!(plan.frames()[0].parent_version(), base.version());
        assert_eq!(plan.tail().resolve(&slot), Some(&updated));
        assert!(
            plan.tail()
                .resolve(&SlotId::new("workspace/rejected"))
                .is_none()
        );
        assert!(
            plan.tail()
                .resolve(&SlotId::new("workspace/validator-rejected"))
                .is_none()
        );
        assert!(
            plan.tail()
                .resolve(&SlotId::new("workspace/accepted"))
                .is_some()
        );
        let mut frames = base.frames();
        frames.extend(plan.frames().iter().cloned());
        let reconstructed = World::reconstruct(&frames).expect("epoch frame reconstructs");
        assert_eq!(reconstructed.id(), plan.tail().id());
        assert_eq!(reconstructed.next_entity(), plan.tail().next_entity());
    }

    #[test]
    fn empty_accepted_intent_still_materializes_an_epoch_world() {
        let base = Arc::new(World::genesis());
        let plan = derive_epoch_world(base.clone(), vec![QueuedIntent::new()], &[]);
        assert!(plan.outcomes()[0].accepted().is_some());
        assert_eq!(plan.frames().len(), 1);
        assert_eq!(plan.tail().version(), 1);
        assert_ne!(plan.tail().id(), base.id());
    }

    #[test]
    fn all_rejected_epoch_keeps_the_base_and_emits_no_frame() {
        let base = Arc::new(World::genesis());
        let mut intent = QueuedIntent::new();
        intent.expect_world(WorldId::new(7));
        intent.define_fact(
            SlotId::new("rejected/all"),
            Fact::new(
                Atom::Literal(Literal::new("no")),
                Predicate::new("state"),
                Atom::Literal(Literal::new("never")),
            ),
        );
        let plan = derive_epoch_world(base.clone(), vec![intent], &[]);
        assert!(plan.outcomes()[0].rejected().is_some());
        assert!(plan.frames().is_empty());
        assert_eq!(plan.tail().id(), base.id());
        assert!(plan.tail().resolve(&SlotId::new("rejected/all")).is_none());
    }

    #[test]
    fn epoch_workspace_keeps_private_predecessor_identity_for_expected_world() {
        let base = Arc::new(World::genesis());
        let mut first = QueuedIntent::new();
        first.define_fact(
            SlotId::new("private/first"),
            Fact::new(
                Atom::Literal(Literal::new("first")),
                Predicate::new("state"),
                Atom::Literal(Literal::new("ready")),
            ),
        );
        let private_first = derive_epoch(base.clone(), vec![first.clone()], &[]).tail();

        let mut second = QueuedIntent::new();
        second.expect_world(private_first.id());
        second.define_fact(
            SlotId::new("private/second"),
            Fact::new(
                Atom::Literal(Literal::new("second")),
                Predicate::new("state"),
                Atom::Literal(Literal::new("ready")),
            ),
        );

        let plan = derive_epoch_world(base, vec![first, second], &[]);
        assert!(
            plan.outcomes()
                .iter()
                .all(|outcome| outcome.accepted().is_some())
        );
        assert_eq!(plan.frames().len(), 1);
        let first = plan.outcomes()[0].accepted().unwrap();
        let second = plan.outcomes()[1].accepted().unwrap();
        assert_eq!(first.world().id(), second.world().id());
        assert_eq!(first.frame(), second.frame());
    }

    #[test]
    fn epoch_workspace_validators_observe_the_sequential_candidate_chain() {
        use std::sync::Mutex;

        type Observation = (WorldId, WorldId, u64, u64, Vec<Operation>);

        fn recording_validator(observations: Arc<Mutex<Vec<Observation>>>) -> Validator {
            Arc::new(move |candidate| {
                observations.lock().unwrap().push((
                    candidate.base_world(),
                    candidate.id(),
                    candidate.version(),
                    candidate.next_entity(),
                    candidate.operations().to_vec(),
                ));
                Ok(())
            })
        }

        let mut intents = Vec::new();
        for index in 0..3 {
            let mut intent = QueuedIntent::new();
            let entity = intent.entity();
            intent.define(
                SlotId::new(format!("validator/{index}")),
                intent_state_fact(entity, &index.to_string()),
            );
            intents.push(intent);
        }

        let expected = Arc::new(Mutex::new(Vec::new()));
        derive_epoch(
            Arc::new(World::genesis()),
            intents.clone(),
            &[recording_validator(expected.clone())],
        );
        let actual = Arc::new(Mutex::new(Vec::new()));
        derive_epoch_world(
            Arc::new(World::genesis()),
            intents,
            &[recording_validator(actual.clone())],
        );

        assert_eq!(*actual.lock().unwrap(), *expected.lock().unwrap());
    }

    #[test]
    fn epoch_workspace_matches_the_legacy_sequential_collapse() {
        let base = Arc::new(World::genesis());
        let mut seed = 0xbb67_ae85_84ca_a73b_u64;
        let mut intents = Vec::with_capacity(512);
        for index in 0..512_u64 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let mut intent = QueuedIntent::new();
            if index % 19 == 0 {
                intent.expect_world(WorldId::new(7));
            }
            if index % 3 == 0 {
                let entity = intent.entity();
                intent.define(
                    SlotId::new(format!("differential/entity/{index}")),
                    intent_state_fact(entity, &seed.to_string()),
                );
            }
            let slot = SlotId::new(format!("differential/slot/{}", seed % 41));
            if seed & 7 == 0 {
                intent.forget(slot);
            } else {
                intent.define_fact(
                    slot,
                    Fact::new(
                        Atom::Literal(Literal::new("differential")),
                        Predicate::new("value"),
                        Atom::Literal(Literal::new(format!("{index}:{seed}"))),
                    ),
                );
            }
            intents.push(intent);
        }

        let sequential = derive_epoch(base.clone(), intents.clone(), &[]);
        let operations = sequential
            .frames()
            .iter()
            .flat_map(|frame| frame.operations().iter().cloned())
            .collect();
        let reference_candidate = CandidateWorld::construct(base.as_ref(), operations).unwrap();
        let reference_frame = reference_candidate.commit_frame();
        let reference_world =
            Arc::new(reference_candidate.into_world(reference_frame.clone(), base.history.clone()));

        let workspace = derive_epoch_world(base, intents, &[]);
        assert_eq!(workspace.frames(), &[reference_frame]);
        assert_eq!(workspace.tail().id(), reference_world.id());
        assert_eq!(
            workspace.tail().next_entity(),
            reference_world.next_entity()
        );
        assert_eq!(workspace.outcomes().len(), sequential.outcomes().len());
        for (actual, expected) in workspace.outcomes().iter().zip(sequential.outcomes()) {
            assert_eq!(actual.accepted().is_some(), expected.accepted().is_some());
            if let (Some(actual), Some(expected)) = (actual.accepted(), expected.accepted()) {
                assert_eq!(actual.entities(), expected.entities());
            }
        }
        for slot in 0..41 {
            let slot = SlotId::new(format!("differential/slot/{slot}"));
            assert_eq!(
                workspace.tail().resolve(&slot),
                reference_world.resolve(&slot)
            );
        }
    }

    #[test]
    fn queued_plan_matches_strict_sequential_worlds_and_frames() {
        let mut intents = Vec::new();
        for index in 0..128 {
            let mut intent = QueuedIntent::new();
            let entity = intent.entity();
            intent.define(
                SlotId::new(format!("service/{index}/state")),
                intent_state_fact(entity, &format!("v{index}")),
            );
            if index % 7 == 0 {
                intent.forget(SlotId::new(format!("service/{index}/state")));
            }
            intents.push(intent);
        }

        let plan = derive_epoch(Arc::new(World::genesis()), intents, &[]);
        let oracle = Database::new(MemoryCommitStore::new()).expect("genesis valid");
        let mut oracle_worlds = Vec::new();
        for frame in plan.frames() {
            oracle_worlds.push(replay_frame(&oracle, frame));
        }

        assert_eq!(oracle.frames(), plan.frames());
        assert_eq!(oracle.snapshot().id(), plan.tail().id());
        for (outcome, oracle_world) in plan
            .outcomes()
            .iter()
            .filter_map(EpochOutcome::accepted)
            .zip(oracle_worlds)
        {
            assert_eq!(outcome.world().id(), oracle_world.id());
            assert_eq!(outcome.world().next_entity(), oracle_world.next_entity());
        }
    }

    #[test]
    fn in_memory_epoch_appends_all_frames_then_publishes_only_the_tail() {
        let database = Database::new(MemoryCommitStore::new()).expect("genesis valid");
        let old = database.snapshot();
        let stale = database.begin();
        let mut intents = Vec::new();
        for index in 0..16 {
            let mut intent = QueuedIntent::new();
            intent.define_fact(
                SlotId::new(format!("epoch/{index}")),
                Fact::new(
                    Atom::Literal(Literal::new("epoch")),
                    Predicate::new("value"),
                    Atom::Literal(Literal::new(index.to_string())),
                ),
            );
            intents.push(intent);
        }

        let plan = database.commit_queued_epoch(intents);
        assert_eq!(old.version(), 0);
        assert_eq!(database.snapshot().id(), plan.tail().id());
        assert_eq!(database.snapshot().version(), 16);
        assert_eq!(database.frame_count(), 16);
        assert_eq!(database.frames(), plan.frames());
        assert!(matches!(
            database.commit(stale),
            Err(CommitError::StaleTransaction { .. })
        ));
    }

    #[test]
    fn canonical_file_bytes_match_strict_sequential_execution() {
        let mut intents = Vec::new();
        for index in 0..64 {
            let mut intent = QueuedIntent::new();
            let entity = intent.entity();
            intent.define(
                SlotId::new(format!("canonical/{index}")),
                intent_state_fact(entity, &index.to_string()),
            );
            intents.push(intent);
        }
        let plan = derive_epoch(Arc::new(World::genesis()), intents, &[]);

        let queued_path = temporary_path("queued");
        let strict_path = temporary_path("strict");
        {
            let mut store = FileCommitStore::open(&queued_path).expect("queued file opens");
            for frame in plan.frames() {
                store.append(frame.clone()).expect("frame appends");
            }
        }
        {
            let store = FileCommitStore::open(&strict_path).expect("strict file opens");
            let database = Database::new(store).expect("strict database opens");
            for frame in plan.frames() {
                let mut transaction = database.begin();
                for operation in frame.operations() {
                    match operation {
                        Operation::AllocateEntity { entity } => {
                            assert_eq!(transaction.entity(), *entity);
                        }
                        Operation::Define { slot, fact } => {
                            transaction.define(slot.clone(), fact.clone());
                        }
                        Operation::Forget { slot } => transaction.forget(slot.clone()),
                    }
                }
                database
                    .commit(transaction)
                    .expect("strict commit succeeds");
            }
        }

        assert_eq!(
            fs::read(&queued_path).unwrap(),
            fs::read(&strict_path).unwrap()
        );
        let _ = fs::remove_file(queued_path);
        let _ = fs::remove_file(strict_path);
    }

    #[test]
    fn deterministic_randomized_epoch_matches_sequential_oracle() {
        let mut seed = 0x6a09_e667_f3bc_c909_u64;
        let mut intents = Vec::with_capacity(10_000);
        for index in 0..10_000_u64 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let slot_index = seed % 257;
            let mut intent = QueuedIntent::new();
            if seed & 7 == 0 {
                intent.forget(SlotId::new(format!("random/{slot_index}")));
            } else {
                intent.define_fact(
                    SlotId::new(format!("random/{slot_index}")),
                    Fact::new(
                        Atom::Literal(Literal::new("random")),
                        Predicate::new("value"),
                        Atom::Literal(Literal::new(format!("{index}:{seed}"))),
                    ),
                );
            }
            intents.push(intent);
        }

        let plan = derive_epoch(Arc::new(World::genesis()), intents, &[]);
        let oracle = Database::new(MemoryCommitStore::new()).expect("genesis valid");
        for frame in plan.frames() {
            replay_frame(&oracle, frame);
        }
        assert_eq!(oracle.frames(), plan.frames());
        assert_eq!(oracle.snapshot().id(), plan.tail().id());
        assert_eq!(oracle.snapshot().record_count(), plan.tail().record_count());
        for slot_index in 0..257 {
            let slot = SlotId::new(format!("random/{slot_index}"));
            assert_eq!(oracle.snapshot().resolve(&slot), plan.tail().resolve(&slot));
        }
    }

    #[test]
    fn token_vm_materializer_matches_epoch_worlds_across_boundaries() {
        let mut materializer = VmEpochMaterializer::new(1);
        let mut reference_world = Arc::new(World::genesis());
        let mut vm_world = Arc::new(World::genesis());

        for epoch in 0..64_u64 {
            let slot = SlotId::new(format!("vm/epoch/{epoch}"));
            let fact = Fact::new(
                Atom::Literal(Literal::new("vm")),
                Predicate::new("value"),
                Atom::Literal(Literal::new(epoch.to_string())),
            );
            let mut define = QueuedIntent::new();
            define.expect_absent(slot.clone());
            define.define_fact(slot.clone(), fact.clone());

            let mut dependent = QueuedIntent::new();
            dependent.expect_value(slot.clone(), fact);
            dependent.define_fact(
                SlotId::new(format!("vm/dependent/{epoch}")),
                Fact::new(
                    Atom::Literal(Literal::new("dependent")),
                    Predicate::new("value"),
                    Atom::Literal(Literal::new(epoch.to_string())),
                ),
            );

            let mut rejected = QueuedIntent::new();
            rejected.expect_absent(slot.clone());
            let rejected_entity = rejected.entity();
            rejected.define(
                SlotId::new(format!("vm/rejected/{epoch}")),
                intent_state_fact(rejected_entity, "never"),
            );

            let mut allocated = QueuedIntent::new();
            let entity = allocated.entity();
            allocated.define(
                SlotId::new(format!("vm/entity/{epoch}")),
                intent_state_fact(entity, &epoch.to_string()),
            );

            let mut intents = vec![define, dependent, rejected, allocated];
            if epoch > 0 && epoch % 3 == 0 {
                let mut forget = QueuedIntent::new();
                forget.forget(SlotId::new(format!("vm/dependent/{}", epoch - 1)));
                intents.push(forget);
            }

            let expected = derive_epoch_world(reference_world, intents.clone(), &[]);
            let (actual, used_vm) = materializer
                .materialize(vm_world, intents, &[])
                .expect("VM epoch materializes");
            assert!(used_vm);
            assert_eq!(actual.frames(), expected.frames(), "epoch {epoch}");
            assert_eq!(actual.tail().id(), expected.tail().id(), "epoch {epoch}");
            assert_eq!(
                actual.tail().next_entity(),
                expected.tail().next_entity(),
                "epoch {epoch}"
            );
            assert_eq!(
                actual.tail().active_slot_count(),
                expected.tail().active_slot_count(),
                "epoch {epoch}"
            );
            assert_eq!(
                actual.tail().record_count(),
                expected.tail().record_count(),
                "epoch {epoch}"
            );
            assert!(
                !actual.tail().is_query_projection_materialized(),
                "VM epoch {epoch} projected the compatibility kernel eagerly"
            );
            for (actual, expected) in actual.outcomes().iter().zip(expected.outcomes()) {
                assert_eq!(actual.accepted().is_some(), expected.accepted().is_some());
                if let (Some(actual), Some(expected)) = (actual.accepted(), expected.accepted()) {
                    assert_eq!(actual.entities(), expected.entities());
                }
            }
            reference_world = expected.tail();
            vm_world = actual.tail();
        }

        let final_slot = SlotId::new("vm/dependent/63");
        assert_eq!(
            vm_world.resolve(&final_slot),
            reference_world.resolve(&final_slot)
        );
        assert!(vm_world.is_query_projection_materialized());
        assert!(!vm_world.is_legacy_query_projection_materialized());
    }

    #[test]
    fn token_vm_fallback_keeps_validator_epochs_in_sync() {
        let mut materializer = VmEpochMaterializer::new(1);
        let base = Arc::new(World::genesis());
        let mut first = QueuedIntent::new();
        first.define_fact(
            SlotId::new("vm/validated"),
            state_fact(EntityId::new(7), "ready"),
        );
        let validator: Validator = Arc::new(|candidate| {
            candidate
                .resolve(&SlotId::new("vm/validated"))
                .is_some()
                .then_some(())
                .ok_or_else(|| "missing validated state".to_owned())
        });
        let (fallback, used_vm) = materializer
            .materialize(base, vec![first], &[validator])
            .expect("validator fallback materializes");
        assert!(!used_vm);

        let mut dependent = QueuedIntent::new();
        dependent.expect_value(
            SlotId::new("vm/validated"),
            state_fact(EntityId::new(7), "ready"),
        );
        dependent.define_fact(
            SlotId::new("vm/after-validator"),
            state_fact(EntityId::new(7), "still-ready"),
        );
        let (actual, used_vm) = materializer
            .materialize(fallback.tail(), vec![dependent.clone()], &[])
            .expect("VM resumes after validator fallback");
        let expected = derive_epoch_world(fallback.tail(), vec![dependent], &[]);
        assert!(used_vm);
        assert_eq!(actual.frames(), expected.frames());
        assert_eq!(actual.tail().id(), expected.tail().id());
    }

    #[test]
    fn global_token_isolation_across_semantic_intents() {
        use crate::transaction_ast::{AtomRef, SemanticIntent, TransactionAST, TransactionOp};

        let mut materializer = VmEpochMaterializer::new(1);
        let base = Arc::new(World::genesis());

        // Intent 1 uses "book/status"
        let ast1 = TransactionAST::new(
            10,
            vec![
                TransactionOp::Allocate {
                    result: "b1".to_owned(),
                },
                TransactionOp::Define {
                    slot: "book/status".to_owned(),
                    subject: AtomRef::Symbol("b1".to_owned()),
                    predicate: "is".to_owned(),
                    object: AtomRef::Literal("available".to_owned()),
                },
            ],
        );
        let intent1 =
            crate::queued_controller::ControllerIntent::Semantic(SemanticIntent::new(ast1));

        // Intent 2 uses "patron/name"
        let ast2 = TransactionAST::new(
            11,
            vec![
                TransactionOp::Allocate {
                    result: "p1".to_owned(),
                },
                TransactionOp::Define {
                    slot: "patron/name".to_owned(),
                    subject: AtomRef::Symbol("p1".to_owned()),
                    predicate: "is".to_owned(),
                    object: AtomRef::Literal("alice".to_owned()),
                },
            ],
        );
        let intent2 =
            crate::queued_controller::ControllerIntent::Semantic(SemanticIntent::new(ast2));

        let plan = materializer.materialize_mixed(base.clone(), vec![intent1, intent2], &[]);
        assert_eq!(plan.outcomes().len(), 2);
        assert!(matches!(
            plan.outcomes()[0],
            MixedEpochOutcome::SemanticAccepted(_)
        ));
        assert!(matches!(
            plan.outcomes()[1],
            MixedEpochOutcome::SemanticAccepted(_)
        ));

        // Verify distinct slot tokens in materializer
        let slot_book = materializer.slots.get(&SlotId::new("book/status")).unwrap();
        let slot_patron = materializer.slots.get(&SlotId::new("patron/name")).unwrap();
        assert_ne!(slot_book, slot_patron);

        // Verify resolution in published tail world
        let tail = plan.tail();
        let book_val = tail
            .resolve(&SlotId::new("book/status"))
            .expect("book status resolved");
        let patron_val = tail
            .resolve(&SlotId::new("patron/name"))
            .expect("patron name resolved");
        assert_eq!(book_val.object, Atom::Literal(Literal::new("available")));
        assert_eq!(patron_val.object, Atom::Literal(Literal::new("alice")));
    }

    #[test]
    fn three_intent_mixed_batch_validator_rollback() {
        use crate::transaction_ast::{AtomRef, SemanticIntent, TransactionAST, TransactionOp};

        let mut materializer = VmEpochMaterializer::new(1);
        let base = Arc::new(World::genesis());

        // 1. QueuedIntent defines A ("slot_a" = "val_a")
        let mut intent1 = QueuedIntent::new();
        intent1.define_fact(
            SlotId::new("slot_a"),
            state_fact(EntityId::new(10), "val_a"),
        );
        let c_intent1 = crate::queued_controller::ControllerIntent::Queued(intent1);

        // 2. SemanticIntent reads A ("slot_a"), defines B ("slot_b" = "val_b")
        let ast2 = TransactionAST::new(
            20,
            vec![
                TransactionOp::ExpectObject {
                    slot: "slot_a".to_owned(),
                    expected: AtomRef::Literal("val_a".to_owned()),
                },
                TransactionOp::Allocate {
                    result: "b_ent".to_owned(),
                },
                TransactionOp::Define {
                    slot: "slot_b".to_owned(),
                    subject: AtomRef::Symbol("b_ent".to_owned()),
                    predicate: "is".to_owned(),
                    object: AtomRef::Literal("val_b".to_owned()),
                },
            ],
        );
        let c_intent2 =
            crate::queued_controller::ControllerIntent::Semantic(SemanticIntent::new(ast2));

        // 3. SemanticIntent defines C ("slot_c_failed" = "val_c")
        let ast3 = TransactionAST::new(
            30,
            vec![
                TransactionOp::Allocate {
                    result: "c_ent".to_owned(),
                },
                TransactionOp::Define {
                    slot: "slot_c_failed".to_owned(),
                    subject: AtomRef::Symbol("c_ent".to_owned()),
                    predicate: "is".to_owned(),
                    object: AtomRef::Literal("val_c".to_owned()),
                },
            ],
        );
        let c_intent3 =
            crate::queued_controller::ControllerIntent::Semantic(SemanticIntent::new(ast3));

        // Host validator that rejects any candidate world defining "slot_c_failed"
        let validator: Validator = Arc::new(|candidate| {
            if candidate.resolve(&SlotId::new("slot_c_failed")).is_some() {
                Err("rejected by host validator".to_owned())
            } else {
                Ok(())
            }
        });

        let allocator_before = materializer.workspace.next_entity();
        let plan = materializer.materialize_mixed(
            base.clone(),
            vec![c_intent1, c_intent2, c_intent3],
            &[validator],
        );

        assert_eq!(plan.outcomes().len(), 3);
        assert!(
            matches!(plan.outcomes()[0], MixedEpochOutcome::QueuedAccepted(_)),
            "Intent 1 accepted"
        );
        assert!(
            matches!(plan.outcomes()[1], MixedEpochOutcome::SemanticAccepted(_)),
            "Intent 2 accepted and observed Intent 1"
        );
        assert!(
            matches!(plan.outcomes()[2], MixedEpochOutcome::SemanticRejected(_)),
            "Intent 3 rejected by validator"
        );

        let tail = plan.tail();

        // B observed A
        assert_eq!(
            tail.resolve(&SlotId::new("slot_a")).unwrap().object,
            Atom::Literal(Literal::new("val_a"))
        );
        // B committed
        assert_eq!(
            tail.resolve(&SlotId::new("slot_b")).unwrap().object,
            Atom::Literal(Literal::new("val_b"))
        );
        // C is absent
        assert!(tail.resolve(&SlotId::new("slot_c_failed")).is_none());

        // Vocabulary introduced only by C is absent from materializer dictionary
        assert!(
            materializer
                .slots
                .get(&SlotId::new("slot_c_failed"))
                .is_none()
        );

        // Entity allocator is unchanged by C's failed allocation (Intent 2 allocated 1 entity)
        assert_eq!(materializer.workspace.next_entity(), allocator_before + 1);
    }

    #[test]
    fn mixed_batch_epoch_contract_and_versioning() {
        use crate::transaction_ast::{AtomRef, SemanticIntent, TransactionAST, TransactionOp};

        let mut materializer = VmEpochMaterializer::new(1);
        let base = Arc::new(World::genesis());
        assert_eq!(base.version(), 0);

        let ast1 = TransactionAST::new(
            100,
            vec![
                TransactionOp::Allocate {
                    result: "e1".to_owned(),
                },
                TransactionOp::Define {
                    slot: "slot_x".to_owned(),
                    subject: AtomRef::Symbol("e1".to_owned()),
                    predicate: "is".to_owned(),
                    object: AtomRef::Literal("v1".to_owned()),
                },
            ],
        );

        let ast2 = TransactionAST::new(
            101,
            vec![
                TransactionOp::ExpectObject {
                    slot: "slot_x".to_owned(),
                    expected: AtomRef::Literal("v1".to_owned()),
                },
                TransactionOp::Allocate {
                    result: "e2".to_owned(),
                },
                TransactionOp::Define {
                    slot: "slot_y".to_owned(),
                    subject: AtomRef::Symbol("e2".to_owned()),
                    predicate: "is".to_owned(),
                    object: AtomRef::Literal("v2".to_owned()),
                },
            ],
        );

        let plan = materializer.materialize_mixed(
            base.clone(),
            vec![
                crate::queued_controller::ControllerIntent::Semantic(SemanticIntent::new(ast1)),
                crate::queued_controller::ControllerIntent::Semantic(SemanticIntent::new(ast2)),
            ],
            &[],
        );

        // 1. Nonempty accepted batch advances version exactly once
        assert_eq!(plan.tail().version(), 1);
        assert_eq!(plan.frames().len(), 1);

        // 2. Both accepted tickets share the exact same tail world ID and version
        let w1 = match &plan.outcomes()[0] {
            MixedEpochOutcome::SemanticAccepted(a) => a.world(),
            _ => panic!("expected semantic accepted"),
        };
        let w2 = match &plan.outcomes()[1] {
            MixedEpochOutcome::SemanticAccepted(a) => a.world(),
            _ => panic!("expected semantic accepted"),
        };
        assert_eq!(w1.id(), w2.id());
        assert_eq!(w1.version(), 1);
        assert_eq!(w2.version(), 1);

        // 3. Precondition in intent 2 saw intent 1's state
        assert_eq!(
            w2.resolve(&SlotId::new("slot_y")).unwrap().object,
            Atom::Literal(Literal::new("v2"))
        );

        // 4. All-rejected batch advances version by 0 and entity allocator by 0
        let ast_bad = TransactionAST::new(
            102,
            vec![TransactionOp::ExpectObject {
                slot: "slot_x".to_owned(),
                expected: AtomRef::Literal("nonexistent".to_owned()),
            }],
        );

        let next_ent_before = materializer.workspace.next_entity();
        let plan_bad = materializer.materialize_mixed(
            plan.tail(),
            vec![crate::queued_controller::ControllerIntent::Semantic(
                SemanticIntent::new(ast_bad),
            )],
            &[],
        );

        assert_eq!(plan_bad.tail().version(), 1);
        assert_eq!(plan_bad.frames().len(), 0);
        assert_eq!(materializer.workspace.next_entity(), next_ent_before);
    }

    #[test]
    fn non_genesis_bootstrap_semantic_intent() {
        use crate::transaction_ast::{AtomRef, SemanticIntent, TransactionAST, TransactionOp};

        let db = Database::new(MemoryCommitStore::new()).unwrap();
        let mut tx = Transaction::new(db.snapshot());
        let e_existing = tx.entity();
        tx.define(
            SlotId::new("existing/slot"),
            Fact::new(
                Atom::Entity(e_existing),
                Predicate::new("has"),
                Atom::Literal(Literal::new("value1")),
            ),
        );
        let base_world = db.commit(tx).unwrap();
        assert!(base_world.version() > 0);

        let mut materializer = VmEpochMaterializer::new(1);
        materializer.synchronize_to(&base_world).unwrap();

        let ast = TransactionAST::new(
            500,
            vec![
                TransactionOp::ExpectObject {
                    slot: "existing/slot".to_owned(),
                    expected: AtomRef::Literal("value1".to_owned()),
                },
                TransactionOp::Allocate {
                    result: "new_ent".to_owned(),
                },
                TransactionOp::Define {
                    slot: "existing/slot".to_owned(),
                    subject: AtomRef::Symbol("new_ent".to_owned()),
                    predicate: "has".to_owned(),
                    object: AtomRef::Literal("value2".to_owned()),
                },
            ],
        );

        let plan = materializer.materialize_mixed(
            base_world.clone(),
            vec![crate::queued_controller::ControllerIntent::Semantic(
                SemanticIntent::new(ast),
            )],
            &[],
        );

        assert_eq!(plan.outcomes().len(), 1);
        assert!(matches!(
            plan.outcomes()[0],
            MixedEpochOutcome::SemanticAccepted(_)
        ));
        let tail = plan.tail();
        let updated = tail.resolve(&SlotId::new("existing/slot")).unwrap();
        assert_eq!(updated.object, Atom::Literal(Literal::new("value2")));
    }

    #[test]
    fn strict_commit_between_controller_batches() {
        use crate::transaction_ast::{AtomRef, SemanticIntent, TransactionAST, TransactionOp};

        let db = Database::new(MemoryCommitStore::new()).unwrap();
        let mut materializer = VmEpochMaterializer::new(1);

        // Batch A
        let ast_a = TransactionAST::new(
            1,
            vec![
                TransactionOp::Allocate {
                    result: "a".to_owned(),
                },
                TransactionOp::Define {
                    slot: "batch_a/slot".to_owned(),
                    subject: AtomRef::Symbol("a".to_owned()),
                    predicate: "is".to_owned(),
                    object: AtomRef::Literal("alpha".to_owned()),
                },
            ],
        );
        let plan_a = db.commit_mixed_epoch(
            vec![crate::queued_controller::ControllerIntent::Semantic(
                SemanticIntent::new(ast_a),
            )],
            &mut materializer,
        );
        assert_eq!(plan_a.tail().version(), 1);

        // Strict commit without allocating an entity
        let mut tx_strict = Transaction::new(db.snapshot());
        tx_strict.define(
            SlotId::new("strict/slot"),
            Fact::new(
                Atom::Entity(EntityId::new(1)),
                Predicate::new("is"),
                Atom::Literal(Literal::new("strict_val")),
            ),
        );
        let strict_world = db.commit(tx_strict).unwrap();
        assert_eq!(strict_world.version(), 2);

        // Batch B must observe strict commit after synchronize_to
        materializer.synchronize_to(&db.snapshot()).unwrap();
        let ast_b = TransactionAST::new(
            2,
            vec![
                TransactionOp::ExpectObject {
                    slot: "strict/slot".to_owned(),
                    expected: AtomRef::Literal("strict_val".to_owned()),
                },
                TransactionOp::Define {
                    slot: "batch_b/slot".to_owned(),
                    subject: AtomRef::Entity(crate::transaction_ast::EntityId(1)),
                    predicate: "is".to_owned(),
                    object: AtomRef::Literal("beta".to_owned()),
                },
            ],
        );
        let plan_b = db.commit_mixed_epoch(
            vec![crate::queued_controller::ControllerIntent::Semantic(
                SemanticIntent::new(ast_b),
            )],
            &mut materializer,
        );

        assert_eq!(plan_b.outcomes().len(), 1);
        assert!(matches!(
            plan_b.outcomes()[0],
            MixedEpochOutcome::SemanticAccepted(_)
        ));
        assert_eq!(plan_b.tail().version(), 3);
    }

    #[test]
    fn mapped_base_vocabulary_isolation() {
        use crate::transaction_ast::{AtomRef, SemanticIntent, TransactionAST, TransactionOp};

        let path = temporary_path("mapped-vocab-iso");
        let snapshot_path = MmapVmSnapshot::path_for(&path);
        let world_snap = {
            let db = Database::new(FileCommitStore::open(&path).unwrap()).unwrap();
            let mut tx = Transaction::new(db.snapshot());
            let e = tx.entity();
            tx.define(
                SlotId::new("mapped/slot"),
                Fact::new(
                    Atom::Entity(e),
                    Predicate::new("p_mapped"),
                    Atom::Literal(Literal::new("l_mapped")),
                ),
            );
            db.commit(tx).unwrap()
        };

        world_snap.materialize_query_projection();
        let bytes = std::fs::read(&path).unwrap();
        MmapVmSnapshot::create(&path, &bytes, 1, &world_snap, world_snap.vm_query()).unwrap();

        let mmap_snapshot = MmapVmSnapshot::open(&path, &bytes).unwrap();
        let mut materializer = VmEpochMaterializer::from_mmap(mmap_snapshot.clone()).unwrap();
        let base = World::from_mmap(mmap_snapshot.clone());

        // 1. Live committed slot
        let ast_live = TransactionAST::new(
            10,
            vec![
                TransactionOp::Allocate {
                    result: "e1".to_owned(),
                },
                TransactionOp::Define {
                    slot: "live/slot".to_owned(),
                    subject: AtomRef::Symbol("e1".to_owned()),
                    predicate: "p_live".to_owned(),
                    object: AtomRef::Literal("l_live".to_owned()),
                },
            ],
        );
        let plan_live = materializer.materialize_mixed(
            base.clone(),
            vec![crate::queued_controller::ControllerIntent::Semantic(
                SemanticIntent::new(ast_live),
            )],
            &[],
        );
        assert!(matches!(
            plan_live.outcomes()[0],
            MixedEpochOutcome::SemanticAccepted(_)
        ));

        // 2. Rejected intent proposing 3rd slot
        let ast_rejected = TransactionAST::new(
            11,
            vec![
                TransactionOp::ExpectObject {
                    slot: "mapped/slot".to_owned(),
                    expected: AtomRef::Literal("nonexistent".to_owned()),
                },
                TransactionOp::Define {
                    slot: "rejected/slot".to_owned(),
                    subject: AtomRef::Literal("sub".to_owned()),
                    predicate: "p_rejected".to_owned(),
                    object: AtomRef::Literal("l_rejected".to_owned()),
                },
            ],
        );
        let plan_rej = materializer.materialize_mixed(
            plan_live.tail(),
            vec![crate::queued_controller::ControllerIntent::Semantic(
                SemanticIntent::new(ast_rejected),
            )],
            &[],
        );
        assert!(matches!(
            plan_rej.outcomes()[0],
            MixedEpochOutcome::SemanticRejected(_)
        ));

        // Verify vocabulary isolation:
        assert!(
            materializer
                .mapped_base
                .as_ref()
                .unwrap()
                .slot_token("mapped/slot")
                .is_some()
        );
        assert!(materializer.slots.contains_key(&SlotId::new("live/slot")));
        assert!(
            !materializer
                .slots
                .contains_key(&SlotId::new("rejected/slot"))
        );
        assert!(
            materializer
                .mapped_base
                .as_ref()
                .unwrap()
                .slot_token("rejected/slot")
                .is_none()
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&snapshot_path);
    }

    #[test]
    fn randomized_mixed_epoch_differential_fuzzer() {
        use crate::transaction_ast::{AtomRef, SemanticIntent, TransactionAST, TransactionOp};

        let mut materializer = VmEpochMaterializer::new(1);
        let mut current_world = Arc::new(World::genesis());
        let mut seed = 0x9E3779B97F4A7C15u64;

        let mut next_prng = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seed
        };

        for batch_idx in 0..50 {
            let intent_count = (next_prng() % 8 + 1) as usize;
            let mut intents = Vec::with_capacity(intent_count);
            let mut expected_queued = Vec::with_capacity(intent_count);

            for i in 0..intent_count {
                let is_semantic = (next_prng() % 2) == 0;
                let is_valid = (next_prng() % 4) != 0; // 75% valid, 25% invalid

                let slot_name = format!("fuzz_slot_{}", (next_prng() % 5));
                let val_name = if is_valid {
                    format!("val_{}_{i}", batch_idx)
                } else {
                    "invalid_expected_val".to_owned()
                };

                if is_semantic {
                    let mut ops = Vec::new();
                    if !is_valid {
                        ops.push(TransactionOp::ExpectObject {
                            slot: slot_name.clone(),
                            expected: AtomRef::Literal("definitely_nonexistent_val".to_owned()),
                        });
                    }
                    ops.push(TransactionOp::Allocate {
                        result: "sym1".to_owned(),
                    });
                    ops.push(TransactionOp::Define {
                        slot: slot_name,
                        subject: AtomRef::Symbol("sym1".to_owned()),
                        predicate: "is".to_owned(),
                        object: AtomRef::Literal(val_name),
                    });
                    let ast = TransactionAST::new(batch_idx * 100 + i as u64, ops);
                    intents.push(crate::queued_controller::ControllerIntent::Semantic(
                        SemanticIntent::new(ast),
                    ));
                } else {
                    let mut q = QueuedIntent::new();
                    if !is_valid {
                        q.expect_value(
                            SlotId::new(&slot_name),
                            state_fact(EntityId::new(99999), "bad"),
                        );
                    }
                    let temp = q.entity();
                    q.define(
                        SlotId::new(&slot_name),
                        IntentFact {
                            subject: IntentAtom::Temporary(temp),
                            predicate: Predicate::new("is"),
                            object: IntentAtom::Literal(Literal::new(&val_name)),
                        },
                    );
                    expected_queued.push(q.clone());
                    intents.push(crate::queued_controller::ControllerIntent::Queued(q));
                }
            }

            let mixed_plan = materializer.materialize_mixed(current_world.clone(), intents, &[]);
            current_world = mixed_plan.tail();
        }

        assert!(current_world.version() > 0);
    }

    fn temporary_path(label: &str) -> PathBuf {
        let sequence = TEMP_PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "forthdb-m6a-{label}-{}-{sequence}.db",
            std::process::id()
        ))
    }
}
