use crate::semantic_isa::{InstructionStreamFrame, StreamDictionary};
use crate::stack_vm::{Cell, Instruction, Opcode, SlotToken};
use forthdb_core::{Literal, Predicate, SlotId};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntityId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq)]
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

impl TransactionAST {
    pub fn new(namespace: u64, operations: Vec<TransactionOp>) -> Self {
        Self {
            namespace,
            operations,
        }
    }

    pub fn lower_to_sisa(&self) -> Result<InstructionStreamFrame, String> {
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
                TransactionOp::Allocate { result: "foo".to_string() },
                TransactionOp::Allocate { result: "foo".to_string() },
            ],
        );
        let err = ast.lower_to_sisa().unwrap_err();
        assert_eq!(err, "duplicate allocation for symbol 'foo'");
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
        assert_eq!(err, "use of undefined symbol 'undefined'");
    }
}
