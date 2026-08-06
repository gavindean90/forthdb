//! Versioned Semantic Instruction Stream (Semantic ISA v1) specification and binary framing.
//!
//! Provides deterministic binary encoding and decoding for the instruction stream
//! that frontends generate and the kernel executes.

use crate::stack_vm::{Cell, Instruction, Opcode, SlotToken};
use forthdb_core::{Literal, Predicate, SlotId};
use std::io::{Cursor, Read};

pub const SEMANTIC_ISA_VERSION_1: u32 = 1;
pub const STREAM_MAGIC: &[u8; 4] = b"SISA";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamDictionary {
    pub slots: Vec<(SlotToken, SlotId)>,
    pub predicates: Vec<(Cell, Predicate)>,
    pub literals: Vec<(Cell, Literal)>,
}

impl StreamDictionary {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            predicates: Vec::new(),
            literals: Vec::new(),
        }
    }
}

impl Default for StreamDictionary {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionStreamFrame {
    pub version: u32,
    pub namespace: u64,
    pub dictionary: StreamDictionary,
    pub local_count: u32,
    pub instructions: Vec<Instruction>,
}

impl InstructionStreamFrame {
    pub fn new(
        namespace: u64,
        dictionary: StreamDictionary,
        local_count: u32,
        instructions: Vec<Instruction>,
    ) -> Self {
        Self {
            version: SEMANTIC_ISA_VERSION_1,
            namespace,
            dictionary,
            local_count,
            instructions,
        }
    }

    pub fn instructions(&self) -> &[Instruction] {
        &self.instructions
    }

    pub fn local_count(&self) -> u32 {
        self.local_count
    }
}

pub fn encode_instruction_stream_frame(frame: &InstructionStreamFrame, output: &mut Vec<u8>) {
    output.extend_from_slice(STREAM_MAGIC);
    put_u32(output, frame.version);
    put_u64(output, frame.namespace);

    // Encode dictionary preamble
    put_u32(output, frame.dictionary.slots.len() as u32);
    for (token, slot) in &frame.dictionary.slots {
        put_u32(output, token.0);
        put_string(output, slot.as_str());
    }

    put_u32(output, frame.dictionary.predicates.len() as u32);
    for (cell, pred) in &frame.dictionary.predicates {
        put_u64(output, cell.0);
        put_string(output, pred.as_str());
    }

    put_u32(output, frame.dictionary.literals.len() as u32);
    for (cell, lit) in &frame.dictionary.literals {
        put_u64(output, cell.0);
        put_string(output, lit.as_str());
    }

    // Encode execution body
    put_u32(output, frame.local_count);
    put_u32(output, frame.instructions.len() as u32);
    for inst in &frame.instructions {
        encode_instruction(inst, output);
    }
}

pub fn decode_instruction_stream_frame(
    input: &mut Cursor<&[u8]>,
) -> Result<InstructionStreamFrame, String> {
    let mut magic = [0u8; 4];
    input
        .read_exact(&mut magic)
        .map_err(|e| format!("failed to read stream magic: {e}"))?;
    if &magic != STREAM_MAGIC {
        return Err(format!("invalid instruction stream magic {:?}", magic));
    }

    let version = take_u32(input)?;
    if version != SEMANTIC_ISA_VERSION_1 {
        return Err(format!("unsupported semantic ISA version {version}"));
    }
    let namespace = take_u64(input)?;

    // Decode dictionary preamble
    let slot_count = take_u32(input)? as usize;
    let mut slots = Vec::with_capacity(slot_count);
    for _ in 0..slot_count {
        let token = SlotToken(take_u32(input)?);
        let slot = SlotId::new(take_string(input)?);
        slots.push((token, slot));
    }

    let pred_count = take_u32(input)? as usize;
    let mut predicates = Vec::with_capacity(pred_count);
    for _ in 0..pred_count {
        let cell = Cell(take_u64(input)?);
        let pred = Predicate::new(take_string(input)?);
        predicates.push((cell, pred));
    }

    let lit_count = take_u32(input)? as usize;
    let mut literals = Vec::with_capacity(lit_count);
    for _ in 0..lit_count {
        let cell = Cell(take_u64(input)?);
        let lit = Literal::new(take_string(input)?);
        literals.push((cell, lit));
    }

    let local_count = take_u32(input)?;
    let inst_count = take_u32(input)? as usize;
    let mut instructions = Vec::with_capacity(inst_count);
    for _ in 0..inst_count {
        instructions.push(decode_instruction(input)?);
    }

    Ok(InstructionStreamFrame {
        version,
        namespace,
        dictionary: StreamDictionary {
            slots,
            predicates,
            literals,
        },
        local_count,
        instructions,
    })
}

fn encode_instruction(inst: &Instruction, output: &mut Vec<u8>) {
    output.push(inst.opcode_u8());
    put_u32(output, inst.argument());
    put_u64(output, inst.immediate());
}

fn decode_instruction(input: &mut Cursor<&[u8]>) -> Result<Instruction, String> {
    let opcode_byte = take_u8(input)?;
    let argument = take_u32(input)?;
    let immediate = take_u64(input)?;
    let opcode = Opcode::from_u8(opcode_byte)?;
    Ok(Instruction::raw(opcode, argument, immediate))
}

fn put_u32(output: &mut Vec<u8>, val: u32) {
    output.extend_from_slice(&val.to_le_bytes());
}

fn put_u64(output: &mut Vec<u8>, val: u64) {
    output.extend_from_slice(&val.to_le_bytes());
}

fn put_string(output: &mut Vec<u8>, val: &str) {
    put_u32(output, val.len() as u32);
    output.extend_from_slice(val.as_bytes());
}

fn take_u8(input: &mut Cursor<&[u8]>) -> Result<u8, String> {
    let mut buf = [0u8; 1];
    input
        .read_exact(&mut buf)
        .map_err(|e| format!("read_u8 failed: {e}"))?;
    Ok(buf[0])
}

fn take_u32(input: &mut Cursor<&[u8]>) -> Result<u32, String> {
    let mut buf = [0u8; 4];
    input
        .read_exact(&mut buf)
        .map_err(|e| format!("read_u32 failed: {e}"))?;
    Ok(u32::from_le_bytes(buf))
}

fn take_u64(input: &mut Cursor<&[u8]>) -> Result<u64, String> {
    let mut buf = [0u8; 8];
    input
        .read_exact(&mut buf)
        .map_err(|e| format!("read_u64 failed: {e}"))?;
    Ok(u64::from_le_bytes(buf))
}

fn take_string(input: &mut Cursor<&[u8]>) -> Result<String, String> {
    let len = take_u32(input)? as usize;
    let pos = input.position() as usize;
    let end = pos.checked_add(len).ok_or_else(|| "string length overflow".to_string())?;
    let buf = input.get_ref();
    if end > buf.len() {
        return Err("truncated string in stream".to_owned());
    }
    let s = std::str::from_utf8(&buf[pos..end])
        .map_err(|e| e.to_string())?
        .to_owned();
    input.set_position(end as u64);
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_instruction_stream_frame() {
        let mut dict = StreamDictionary::new();
        dict.slots.push((SlotToken(1), SlotId::new("user/name")));
        dict.predicates.push((Cell(10), Predicate::new("name")));
        dict.literals.push((Cell(100), Literal::new("Alice")));

        let insts = vec![
            Instruction::push(Cell(5)),
            Instruction::define(SlotToken(1)),
        ];

        let frame = InstructionStreamFrame::new(42, dict, 2, insts);

        let mut encoded = Vec::new();
        encode_instruction_stream_frame(&frame, &mut encoded);

        let mut cursor = Cursor::new(encoded.as_slice());
        let decoded = decode_instruction_stream_frame(&mut cursor).unwrap();

        assert_eq!(decoded.version, frame.version);
        assert_eq!(decoded.namespace, frame.namespace);
        assert_eq!(decoded.local_count, frame.local_count);
        assert_eq!(decoded.instructions(), frame.instructions());
    }

    #[test]
    fn test_python_compiled_isa_stream_interop() {
        let python_file = std::path::Path::new("../../artifacts/python_generated_isa.sisa");
        if python_file.exists() {
            let bytes = std::fs::read(python_file).unwrap();
            let mut cursor = std::io::Cursor::new(bytes.as_slice());
            let frame = decode_instruction_stream_frame(&mut cursor).expect("should decode python frame");
            assert_eq!(frame.version, SEMANTIC_ISA_VERSION_1);
            assert_eq!(frame.local_count, 2);
            assert!(!frame.instructions().is_empty());
        }
    }
}
