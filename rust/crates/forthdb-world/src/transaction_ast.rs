use crate::semantic_isa::{InstructionStreamFrame, StreamDictionary};
use crate::stack_vm::{Cell, Instruction, Opcode, SlotToken};
use forthdb_core::{Literal, Predicate, SlotId};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct EntityId(pub u64);

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct WorldId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AtomRef {
    Entity(EntityId),
    Literal(String),
    Symbol(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransactionOp {
    Allocate {
        result: String,
    },
    ExpectWorld {
        expected: WorldId,
    },
    ExpectObject {
        slot: String,
        expected: AtomRef,
    },
    Define {
        slot: String,
        subject: AtomRef,
        predicate: String,
        object: AtomRef,
    },
    Forget {
        slot: String,
    },
    Reject,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionAST {
    pub namespace: u64,
    pub operations: Vec<TransactionOp>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AtomRefView<'a> {
    Entity(EntityId),
    Literal(&'a str),
    Symbol(&'a str),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TransactionOpView<'a> {
    Allocate {
        result: &'a str,
    },
    ExpectWorld {
        expected: WorldId,
    },
    ExpectObject {
        slot: &'a str,
        expected: AtomRefView<'a>,
    },
    Define {
        slot: &'a str,
        subject: AtomRefView<'a>,
        predicate: &'a str,
        object: AtomRefView<'a>,
    },
    Forget {
        slot: &'a str,
    },
    Reject,
}

pub trait OperationSource<'a> {
    fn len(&self) -> usize;
    fn operation(&self, index: usize) -> TransactionOpView<'a>;
}

pub struct OwnedOperationSource<'a>(pub &'a [TransactionOp]);
impl<'a> OperationSource<'a> for OwnedOperationSource<'a> {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn operation(&self, index: usize) -> TransactionOpView<'a> {
        match &self.0[index] {
            TransactionOp::Allocate { result } => TransactionOpView::Allocate { result: result.as_str() },
            TransactionOp::ExpectWorld { expected } => TransactionOpView::ExpectWorld { expected: expected.clone() },
            TransactionOp::ExpectObject { slot, expected } => TransactionOpView::ExpectObject {
                slot: slot.as_str(),
                expected: match expected {
                    AtomRef::Entity(e) => AtomRefView::Entity(e.clone()),
                    AtomRef::Literal(l) => AtomRefView::Literal(l.as_str()),
                    AtomRef::Symbol(s) => AtomRefView::Symbol(s.as_str()),
                },
            },
            TransactionOp::Define { slot, subject, predicate, object } => TransactionOpView::Define {
                slot: slot.as_str(),
                subject: match subject {
                    AtomRef::Entity(e) => AtomRefView::Entity(e.clone()),
                    AtomRef::Literal(l) => AtomRefView::Literal(l.as_str()),
                    AtomRef::Symbol(s) => AtomRefView::Symbol(s.as_str()),
                },
                predicate: predicate.as_str(),
                object: match object {
                    AtomRef::Entity(e) => AtomRefView::Entity(e.clone()),
                    AtomRef::Literal(l) => AtomRefView::Literal(l.as_str()),
                    AtomRef::Symbol(s) => AtomRefView::Symbol(s.as_str()),
                },
            },
            TransactionOp::Forget { slot } => TransactionOpView::Forget { slot: slot.as_str() },
            TransactionOp::Reject => TransactionOpView::Reject,
        }
    }
}

pub struct BorrowedOperationSource<'a>(pub &'a [TransactionOpView<'a>]);
impl<'a> OperationSource<'a> for BorrowedOperationSource<'a> {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn operation(&self, index: usize) -> TransactionOpView<'a> {
        self.0[index]
    }
}

#[derive(Copy, Clone, Debug)]
pub struct TransactionView<'a> {
    pub namespace: u64,
    pub operations: &'a [TransactionOpView<'a>],
}

impl<'a> TransactionView<'a> {
    pub fn borrowed(namespace: u64, operations: &'a [TransactionOpView<'a>]) -> Self {
        Self { namespace, operations }
    }

    pub fn lower_to_sisa_with(
        &self,
        ctx: &mut LoweringContext,
    ) -> Result<InstructionStreamFrame, LoweringError> {
        let source = BorrowedOperationSource(self.operations);
        ctx.lower_from_source(self.namespace, &source)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoweringError {
    DuplicateAllocation(String),
    UndefinedSymbol(String),
    LocalCountOverflow,
    UnsupportedDynamicSymbolExpectObject,
}

impl std::fmt::Display for LoweringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoweringError::DuplicateAllocation(sym) => write!(f, "duplicate allocation for symbol '{}'", sym),
            LoweringError::UndefinedSymbol(sym) => write!(f, "use of undefined symbol '{}'", sym),
            LoweringError::LocalCountOverflow => write!(f, "local count overflow"),
            LoweringError::UnsupportedDynamicSymbolExpectObject => write!(f, "ExpectObject cannot take a dynamic symbol as expected value in SISA v1"),
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum StrRef {
    Alloc(usize),
    ExpectSlot(usize),
    ExpectExpected(usize),
    DefSlot(usize),
    DefSubject(usize),
    DefPredicate(usize),
    DefObject(usize),
    ForgetSlot(usize),
}

impl StrRef {
    fn as_str<'a, S: OperationSource<'a>>(&self, ops: &S) -> &'a str {
        match *self {
            StrRef::Alloc(idx) => match ops.operation(idx) {
                TransactionOpView::Allocate { result } => result,
                _ => unreachable!(),
            },
            StrRef::ExpectSlot(idx) => match ops.operation(idx) {
                TransactionOpView::ExpectObject { slot, .. } => slot,
                _ => unreachable!(),
            },
            StrRef::ExpectExpected(idx) => match ops.operation(idx) {
                TransactionOpView::ExpectObject {
                    expected: AtomRefView::Literal(l),
                    ..
                } => l,
                _ => unreachable!(),
            },
            StrRef::DefSlot(idx) => match ops.operation(idx) {
                TransactionOpView::Define { slot, .. } => slot,
                _ => unreachable!(),
            },
            StrRef::DefSubject(idx) => match ops.operation(idx) {
                TransactionOpView::Define {
                    subject: AtomRefView::Literal(l),
                    ..
                } => l,
                _ => unreachable!(),
            },
            StrRef::DefPredicate(idx) => match ops.operation(idx) {
                TransactionOpView::Define { predicate, .. } => predicate,
                _ => unreachable!(),
            },
            StrRef::DefObject(idx) => match ops.operation(idx) {
                TransactionOpView::Define {
                    object: AtomRefView::Literal(l),
                    ..
                } => l,
                _ => unreachable!(),
            },
            StrRef::ForgetSlot(idx) => match ops.operation(idx) {
                TransactionOpView::Forget { slot } => slot,
                _ => unreachable!(),
            },
        }
    }
}

#[derive(Default)]
pub struct LoweringContext {
    // Intermediate views (safe handles resolving into the AST)
    scratch_slots: Vec<StrRef>,
    scratch_predicates: Vec<StrRef>,
    scratch_literals: Vec<StrRef>,
    scratch_symbols: Vec<StrRef>,

    // Reusable output allocations
    pub instructions: Vec<Instruction>,
    pub dict_slots: Vec<(SlotToken, SlotId)>,
    pub dict_predicates: Vec<(Cell, Predicate)>,
    pub dict_literals: Vec<(Cell, Literal)>,
}

impl LoweringContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.scratch_slots.clear();
        self.scratch_predicates.clear();
        self.scratch_literals.clear();
        self.scratch_symbols.clear();
        self.instructions.clear();
        self.dict_slots.clear();
        self.dict_predicates.clear();
        self.dict_literals.clear();
    }

    pub fn reclaim(&mut self, mut frame: InstructionStreamFrame) {
        self.instructions = std::mem::take(&mut frame.instructions);
        self.dict_slots = std::mem::take(&mut frame.dictionary.slots);
        self.dict_predicates = std::mem::take(&mut frame.dictionary.predicates);
        self.dict_literals = std::mem::take(&mut frame.dictionary.literals);
        self.clear();
    }

    pub fn lower_from_source<'a, S: OperationSource<'a>>(
        &mut self,
        namespace: u64,
        source: &S,
    ) -> Result<InstructionStreamFrame, LoweringError> {
        self.clear();

        for i in 0..source.len() {
            let op = source.operation(i);
            match op {
                TransactionOpView::Allocate { result } => {
                    if self
                        .scratch_symbols
                        .iter()
                        .any(|r| r.as_str(source) == result)
                    {
                        return Err(LoweringError::DuplicateAllocation(result.to_string()));
                    }
                    self.scratch_symbols.push(StrRef::Alloc(i));
                }
                TransactionOpView::ExpectWorld { .. } => {}
                TransactionOpView::ExpectObject { expected, .. } => {
                    self.scratch_slots.push(StrRef::ExpectSlot(i));
                    if let AtomRefView::Literal(_) = expected {
                        self.scratch_literals.push(StrRef::ExpectExpected(i));
                    }
                }
                TransactionOpView::Define {
                    subject, object, ..
                } => {
                    self.scratch_slots.push(StrRef::DefSlot(i));
                    self.scratch_predicates.push(StrRef::DefPredicate(i));
                    if let AtomRefView::Literal(_) = subject {
                        self.scratch_literals.push(StrRef::DefSubject(i));
                    }
                    if let AtomRefView::Literal(_) = object {
                        self.scratch_literals.push(StrRef::DefObject(i));
                    }
                }
                TransactionOpView::Forget { .. } => {
                    self.scratch_slots.push(StrRef::ForgetSlot(i));
                }
                TransactionOpView::Reject => {}
            }
        }

        let local_count = self.scratch_symbols.len() as u32;

        self.scratch_slots
            .sort_unstable_by_key(|&r| r.as_str(source));
        self.scratch_slots
            .dedup_by(|a, b| a.as_str(source) == b.as_str(source));

        self.scratch_predicates
            .sort_unstable_by_key(|&r| r.as_str(source));
        self.scratch_predicates
            .dedup_by(|a, b| a.as_str(source) == b.as_str(source));

        self.scratch_literals
            .sort_unstable_by_key(|&r| r.as_str(source));
        self.scratch_literals
            .dedup_by(|a, b| a.as_str(source) == b.as_str(source));

        for (i, slot_ref) in self.scratch_slots.iter().enumerate() {
            let token = SlotToken(i as u32);
            self.dict_slots
                .push((token, SlotId::new(slot_ref.as_str(source))));
        }

        for (i, pred_ref) in self.scratch_predicates.iter().enumerate() {
            let cell = Cell(i as u64);
            self.dict_predicates
                .push((cell, Predicate::new(pred_ref.as_str(source))));
        }

        let literal_offset = self.scratch_predicates.len();
        for (i, lit_ref) in self.scratch_literals.iter().enumerate() {
            let cell = Cell((i + literal_offset) as u64);
            self.dict_literals
                .push((cell, Literal::new(lit_ref.as_str(source))));
        }

        let load_atom = |atom: &AtomRefView<'a>| -> Result<Instruction, LoweringError> {
            match atom {
                AtomRefView::Entity(e) => Ok(Instruction::push(Cell(e.0))),
                AtomRefView::Literal(l) => {
                    let idx = self
                        .scratch_literals
                        .binary_search_by_key(l, |&r| r.as_str(source))
                        .unwrap();
                    Ok(Instruction::push(Cell((idx + literal_offset) as u64)))
                }
                AtomRefView::Symbol(s) => {
                    if let Some(local) = self
                        .scratch_symbols
                        .iter()
                        .position(|&sym| sym.as_str(source) == *s)
                    {
                        Ok(Instruction::load_local(local as u32))
                    } else {
                        Err(LoweringError::UndefinedSymbol(s.to_string()))
                    }
                }
            }
        };

        for i in 0..source.len() {
            let op = source.operation(i);
            match op {
                TransactionOpView::Allocate { result } => {
                    let local = self
                        .scratch_symbols
                        .iter()
                        .position(|&sym| sym.as_str(source) == result)
                        .unwrap() as u32;
                    self.instructions.push(Instruction::allocate());
                    self.instructions.push(Instruction::store_local(local));
                }
                TransactionOpView::ExpectWorld { expected } => {
                    self.instructions.push(Instruction::raw(
                        Opcode::ExpectObject,
                        u32::MAX,
                        expected.0,
                    ));
                }
                TransactionOpView::ExpectObject { slot, expected } => {
                    let token_idx = self
                        .scratch_slots
                        .binary_search_by_key(&slot, |&r| r.as_str(source))
                        .unwrap() as u32;
                    let cell = match expected {
                        AtomRefView::Entity(e) => Cell(e.0),
                        AtomRefView::Literal(l) => {
                            let idx = self.scratch_literals.binary_search_by_key(&l, |&r| r.as_str(source)).unwrap();
                            Cell((idx + literal_offset) as u64)
                        }
                        AtomRefView::Symbol(_) => return Err(LoweringError::UnsupportedDynamicSymbolExpectObject),
                    };
                    self.instructions.push(Instruction::raw(
                        Opcode::ExpectObject,
                        token_idx,
                        cell.0,
                    ));
                }
                TransactionOpView::Define {
                    slot,
                    subject,
                    predicate,
                    object,
                } => {
                    self.instructions.push(load_atom(&subject)?);
                    let pred_idx = self
                        .scratch_predicates
                        .binary_search_by_key(&predicate, |&r| r.as_str(source))
                        .unwrap();
                    self.instructions
                        .push(Instruction::push(Cell(pred_idx as u64)));
                    self.instructions.push(load_atom(&object)?);
                    let token_idx = self
                        .scratch_slots
                        .binary_search_by_key(&slot, |&r| r.as_str(source))
                        .unwrap() as u32;
                    self.instructions
                        .push(Instruction::define(SlotToken(token_idx)));
                }
                TransactionOpView::Forget { slot } => {
                    let token_idx = self
                        .scratch_slots
                        .binary_search_by_key(&slot, |&r| r.as_str(source))
                        .unwrap() as u32;
                    self.instructions
                        .push(Instruction::forget(SlotToken(token_idx)));
                }
                TransactionOpView::Reject => {
                    self.instructions.push(Instruction::reject());
                }
            }
        }

        let dict = StreamDictionary {
            slots: std::mem::take(&mut self.dict_slots),
            predicates: std::mem::take(&mut self.dict_predicates),
            literals: std::mem::take(&mut self.dict_literals),
        };

        Ok(InstructionStreamFrame::new(
            namespace,
            dict,
            local_count,
            std::mem::take(&mut self.instructions),
        ))
    }
}

impl TransactionAST {
    pub fn new(namespace: u64, operations: Vec<TransactionOp>) -> Self {
        Self {
            namespace,
            operations,
        }
    }

    pub fn lower_to_sisa(&self) -> Result<InstructionStreamFrame, LoweringError> {
        let mut context = LoweringContext::new();
        self.lower_to_sisa_with(&mut context)
    }

    pub fn lower_to_sisa_with(
        &self,
        ctx: &mut LoweringContext,
    ) -> Result<InstructionStreamFrame, LoweringError> {
        let source = OwnedOperationSource(&self.operations);
        ctx.lower_from_source(self.namespace, &source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ast_empty_transaction_golden() {
        let ast = TransactionAST::new(42, vec![]);
        let frame = ast.lower_to_sisa().unwrap();
        assert_eq!(frame.namespace, 42);
        assert_eq!(frame.instructions.len(), 0);
        assert_eq!(frame.local_count, 0);
        assert_eq!(frame.dictionary.slots.len(), 0);
    }

    #[test]
    fn ast_validation_rejects_duplicate_symbol() {
        let ast = TransactionAST::new(
            42,
            vec![
                TransactionOp::Allocate {
                    result: "foo".to_string(),
                },
                TransactionOp::Allocate {
                    result: "foo".to_string(),
                },
            ],
        );
        let err = ast.lower_to_sisa().unwrap_err();
        assert_eq!(err, LoweringError::DuplicateAllocation("foo".to_string()));
    }

    #[test]
    fn ast_validation_rejects_undefined_symbol() {
        let ast = TransactionAST::new(
            42,
            vec![
                TransactionOp::Define {
                    slot: "status".to_string(),
                    subject: AtomRef::Symbol("undefined".to_string()),
                    predicate: "is".to_string(),
                    object: AtomRef::Literal("available".to_string()),
                }
            ],
        );
        let err = ast.lower_to_sisa().unwrap_err();
        assert_eq!(err, LoweringError::UndefinedSymbol("undefined".to_string()));
    }

    // Original lowering implementation preserved for differential testing
    impl TransactionAST {
        pub fn lower_to_sisa_reference(&self) -> Result<InstructionStreamFrame, String> {
            use std::collections::{BTreeMap, BTreeSet};
            
            let mut slots = BTreeSet::new();
            let mut predicates = BTreeSet::new();
            let mut literals = BTreeSet::new();
            let mut symbols = BTreeMap::new();
            let mut local_count = 0u32;

            for op in &self.operations {
                match op {
                    TransactionOp::Allocate { result } => {
                        if symbols.contains_key(result) {
                            return Err(format!("duplicate allocation for symbol '{}'", result));
                        }
                        symbols.insert(result.clone(), local_count);
                        local_count = local_count.checked_add(1).ok_or("local count overflow")?;
                    }
                    TransactionOp::ExpectWorld { .. } => {}
                    TransactionOp::ExpectObject { slot, expected } => {
                        slots.insert(slot.clone());
                        match expected {
                            AtomRef::Literal(l) => {
                                literals.insert(l.clone());
                            }
                            _ => {}
                        }
                    }
                    TransactionOp::Define { slot, subject, predicate, object } => {
                        slots.insert(slot.clone());
                        predicates.insert(predicate.clone());
                        for atom in [subject, object] {
                            match atom {
                                AtomRef::Literal(l) => {
                                    literals.insert(l.clone());
                                }
                                _ => {}
                            }
                        }
                    }
                    TransactionOp::Forget { slot } => {
                        slots.insert(slot.clone());
                    }
                    TransactionOp::Reject => {}
                }
            }

            let mut slot_map = BTreeMap::new();
            let mut dict = StreamDictionary::new();
            
            for (i, slot) in slots.into_iter().enumerate() {
                let token = SlotToken(i as u32);
                slot_map.insert(slot.clone(), token);
                dict.slots.push((token, SlotId::new(&slot)));
            }

            let mut predicate_map = BTreeMap::new();
            for (i, pred) in predicates.into_iter().enumerate() {
                let cell = Cell(i as u64);
                predicate_map.insert(pred.clone(), cell);
                dict.predicates.push((cell, Predicate::new(&pred)));
            }

            let mut literal_map = BTreeMap::new();
            for (i, lit) in literals.into_iter().enumerate() {
                let cell = Cell((i + predicate_map.len()) as u64);
                literal_map.insert(lit.clone(), cell);
                dict.literals.push((cell, Literal::new(&lit)));
            }

            let mut instructions = Vec::new();

            let mut load_atom = |atom: &AtomRef| -> Result<Instruction, String> {
                match atom {
                    AtomRef::Entity(e) => Ok(Instruction::push(Cell(e.0))),
                    AtomRef::Literal(l) => {
                        let cell = literal_map.get(l).unwrap();
                        Ok(Instruction::push(*cell))
                    }
                    AtomRef::Symbol(s) => {
                        if let Some(local) = symbols.get(s) {
                            Ok(Instruction::load_local(*local))
                        } else {
                            Err(format!("use of undefined symbol '{}'", s))
                        }
                    }
                }
            };

            for op in &self.operations {
                match op {
                    TransactionOp::Allocate { result } => {
                        let local = symbols.get(result).unwrap();
                        instructions.push(Instruction::allocate());
                        instructions.push(Instruction::store_local(*local));
                    }
                    TransactionOp::ExpectWorld { expected } => {
                        instructions.push(Instruction::raw(Opcode::ExpectObject, u32::MAX, expected.0));
                    }
                    TransactionOp::ExpectObject { slot, expected } => {
                        let token = slot_map.get(slot).unwrap();
                        let cell = match expected {
                            AtomRef::Entity(e) => Cell(e.0),
                            AtomRef::Literal(l) => *literal_map.get(l).unwrap(),
                            AtomRef::Symbol(_) => return Err("ExpectObject cannot take a dynamic symbol as expected value in SISA v1".to_string()),
                        };
                        instructions.push(Instruction::raw(Opcode::ExpectObject, token.0, cell.0));
                    }
                    TransactionOp::Define { slot, subject, predicate, object } => {
                        instructions.push(load_atom(subject)?);
                        let pred_cell = predicate_map.get(predicate).unwrap();
                        instructions.push(Instruction::push(*pred_cell));
                        instructions.push(load_atom(object)?);
                        let token = slot_map.get(slot).unwrap();
                        instructions.push(Instruction::define(*token));
                    }
                    TransactionOp::Forget { slot } => {
                        let token = slot_map.get(slot).unwrap();
                        instructions.push(Instruction::forget(*token));
                    }
                    TransactionOp::Reject => {
                        instructions.push(Instruction::reject());
                    }
                }
            }

            Ok(InstructionStreamFrame::new(
                self.namespace,
                dict,
                local_count,
                instructions,
            ))
        }
    }

    #[test]
    fn ast_differential_fuzzing() {
        struct Lcg { seed: u32 }
        impl Lcg {
            fn next_u32(&mut self) -> u32 {
                self.seed = self.seed.wrapping_mul(1664525).wrapping_add(1013904223);
                self.seed
            }
            fn next_string(&mut self, opts: &[&str]) -> String {
                let idx = (self.next_u32() as usize) % opts.len();
                opts[idx].to_string()
            }
            fn next_atom(&mut self) -> AtomRef {
                match self.next_u32() % 3 {
                    0 => AtomRef::Entity(EntityId(self.next_u32() as u64 % 100)),
                    1 => AtomRef::Literal(self.next_string(&["a", "b", "c", "d", "e", "f"])),
                    _ => AtomRef::Symbol(self.next_string(&["temp0", "temp1", "temp2"])),
                }
            }
        }

        let mut rng = Lcg { seed: 12345 };

        let slots = ["slot1", "slot2", "slot3", "slot4"];
        let predicates = ["is", "has", "can", "should"];
        let symbols = ["temp0", "temp1", "temp2"];

        let mut ctx = LoweringContext::new();

        for _ in 0..10_000 {
            let op_count = (rng.next_u32() % 20) as usize;
            let mut operations = Vec::with_capacity(op_count);
            
            for _ in 0..op_count {
                let op = match rng.next_u32() % 6 {
                    0 => TransactionOp::Allocate { result: rng.next_string(&symbols) },
                    1 => TransactionOp::ExpectWorld { expected: WorldId(rng.next_u32() as u64) },
                    2 => TransactionOp::ExpectObject { 
                        slot: rng.next_string(&slots), 
                        expected: rng.next_atom() 
                    },
                    3 => TransactionOp::Define {
                        slot: rng.next_string(&slots),
                        subject: rng.next_atom(),
                        predicate: rng.next_string(&predicates),
                        object: rng.next_atom(),
                    },
                    4 => TransactionOp::Forget { slot: rng.next_string(&slots) },
                    _ => TransactionOp::Reject,
                };
                operations.push(op);
            }
            
            let ast = TransactionAST::new(rng.next_u32() as u64, operations);
            
            let ref_res = ast.lower_to_sisa_reference();
            let new_res_oneshot = ast.lower_to_sisa().map_err(|e| e.to_string());
            
            let res_pooled = ast.lower_to_sisa_with(&mut ctx);
            let new_res_pooled = res_pooled.clone().map_err(|e| e.to_string());
            if let Ok(frame) = res_pooled {
                ctx.reclaim(frame);
            } else {
                ctx.clear();
            }

            let view_ops: Vec<TransactionOpView> = (0..op_count)
                .map(|i| OwnedOperationSource(&ast.operations).operation(i))
                .collect();
            let view = TransactionView::borrowed(ast.namespace, &view_ops);
            
            let res_view = view.lower_to_sisa_with(&mut ctx);
            let new_res_view = res_view.clone().map_err(|e| e.to_string());
            if let Ok(frame) = res_view {
                ctx.reclaim(frame);
            } else {
                ctx.clear();
            }
            
            assert_eq!(ref_res, new_res_oneshot);
            assert_eq!(ref_res, new_res_pooled);
            assert_eq!(ref_res, new_res_view);
        }
    }

    #[test]
    fn ast_lowering_exact_output() {
        let mut ops = Vec::new();
        // Intentional out of order alphanumeric insertion to test sorting
        ops.push(TransactionOp::Allocate {
            result: "zebra".to_string(),
        });
        ops.push(TransactionOp::Allocate {
            result: "apple".to_string(),
        });
        ops.push(TransactionOp::Define {
            slot: "gamma/slot".to_string(),
            subject: AtomRef::Symbol("zebra".to_string()),
            predicate: "is".to_string(),
            object: AtomRef::Literal("zoo".to_string()),
        });
        ops.push(TransactionOp::Define {
            slot: "alpha/slot".to_string(),
            subject: AtomRef::Symbol("apple".to_string()),
            predicate: "has".to_string(),
            object: AtomRef::Literal("ant".to_string()),
        });
        ops.push(TransactionOp::ExpectObject {
            slot: "beta/slot".to_string(),
            expected: AtomRef::Literal("zoo".to_string()),
        });

        let ast = TransactionAST::new(100, ops);
        let frame = ast.lower_to_sisa().unwrap();

        // Check slots are sorted
        assert_eq!(frame.dictionary.slots.len(), 3);
        assert_eq!(frame.dictionary.slots[0].1.as_str(), "alpha/slot");
        assert_eq!(frame.dictionary.slots[1].1.as_str(), "beta/slot");
        assert_eq!(frame.dictionary.slots[2].1.as_str(), "gamma/slot");

        // Check predicates are sorted
        assert_eq!(frame.dictionary.predicates.len(), 2);
        assert_eq!(frame.dictionary.predicates[0].1.as_str(), "has");
        assert_eq!(frame.dictionary.predicates[1].1.as_str(), "is");

        // Check literals are sorted
        assert_eq!(frame.dictionary.literals.len(), 2);
        assert_eq!(frame.dictionary.literals[0].1.as_str(), "ant");
        assert_eq!(frame.dictionary.literals[1].1.as_str(), "zoo");

        let mut insts = frame.instructions.iter();

        // Allocate zebra (local 0)
        assert_eq!(insts.next().unwrap().opcode(), Opcode::Allocate);
        let inst = insts.next().unwrap();
        assert_eq!(inst.opcode(), Opcode::StoreLocal);
        assert_eq!(inst.argument(), 0); // zebra

        // Allocate apple (local 1)
        assert_eq!(insts.next().unwrap().opcode(), Opcode::Allocate);
        let inst = insts.next().unwrap();
        assert_eq!(inst.opcode(), Opcode::StoreLocal);
        assert_eq!(inst.argument(), 1); // apple

        // Define gamma/slot
        let inst = insts.next().unwrap(); // load zebra
        assert_eq!(inst.opcode(), Opcode::LoadLocal);
        assert_eq!(inst.argument(), 0);

        let inst = insts.next().unwrap(); // push predicate 'is' (cell 1)
        assert_eq!(inst.opcode(), Opcode::PushCell);
        assert_eq!(inst.immediate(), 1);

        let inst = insts.next().unwrap(); // push literal 'zoo'
        assert_eq!(inst.opcode(), Opcode::PushCell);
        // Literal cell IDs offset by predicate count (2). 'zoo' is index 1, so 2 + 1 = 3
        assert_eq!(inst.immediate(), 3);

        let inst = insts.next().unwrap(); // define gamma/slot (token 2)
        assert_eq!(inst.opcode(), Opcode::Define);
        assert_eq!(inst.argument(), 2);
    }
}
