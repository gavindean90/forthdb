from __future__ import annotations

import struct
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Tuple, Union

STREAM_MAGIC = b"SISA"
SEMANTIC_ISA_VERSION_1 = 1
VM_LITERAL_BASE = 1 << 63


class Opcode:
    EXPECT_OBJECT = 0
    ALLOCATE = 1
    ALLOCATE_DISCARD = 2
    LOAD_LOCAL = 3
    STORE_LOCAL = 4
    PUSH_CELL = 5
    DEFINE = 6
    FORGET = 7
    REJECT = 8


@dataclass(frozen=True)
class Instruction:
    opcode: int
    argument: int = 0
    immediate: int = 0

    def encode(self) -> bytes:
        return struct.pack("<BIQ", self.opcode, self.argument, self.immediate)

    @classmethod
    def decode(cls, data: bytes, offset: int = 0) -> Tuple[Instruction, int]:
        opcode, arg, imm = struct.unpack_from("<BIQ", data, offset)
        return cls(opcode=opcode, argument=arg, immediate=imm), offset + 13


@dataclass(frozen=True)
class TempEntity:
    namespace: int
    index: int


@dataclass(frozen=True)
class EntityRef:
    value: int


@dataclass(frozen=True)
class LiteralRef:
    value: str


AtomRef = Union[EntityRef, TempEntity, LiteralRef]


@dataclass
class StreamDictionary:
    slots: List[Tuple[int, str]] = field(default_factory=list)
    predicates: List[Tuple[int, str]] = field(default_factory=list)
    literals: List[Tuple[int, str]] = field(default_factory=list)

    def encode(self) -> bytes:
        buf = bytearray()
        # Slots
        buf.extend(struct.pack("<I", len(self.slots)))
        for token, slot_str in self.slots:
            slot_bytes = slot_str.encode("utf-8")
            buf.extend(struct.pack("<II", token, len(slot_bytes)))
            buf.extend(slot_bytes)

        # Predicates
        buf.extend(struct.pack("<I", len(self.predicates)))
        for cell_val, pred_str in self.predicates:
            pred_bytes = pred_str.encode("utf-8")
            buf.extend(struct.pack("<QI", cell_val, len(pred_bytes)))
            buf.extend(pred_bytes)

        # Literals
        buf.extend(struct.pack("<I", len(self.literals)))
        for cell_val, lit_str in self.literals:
            lit_bytes = lit_str.encode("utf-8")
            buf.extend(struct.pack("<QI", cell_val, len(lit_bytes)))
            buf.extend(lit_bytes)

        return bytes(buf)

    @classmethod
    def decode(cls, data: bytes, offset: int) -> Tuple[StreamDictionary, int]:
        # Slots
        slot_count, = struct.unpack_from("<I", data, offset)
        offset += 4
        slots = []
        for _ in range(slot_count):
            token, slen = struct.unpack_from("<II", data, offset)
            offset += 8
            s_str = data[offset:offset + slen].decode("utf-8")
            offset += slen
            slots.append((token, s_str))

        # Predicates
        pred_count, = struct.unpack_from("<I", data, offset)
        offset += 4
        predicates = []
        for _ in range(pred_count):
            cell_val, plen = struct.unpack_from("<QI", data, offset)
            offset += 12
            p_str = data[offset:offset + plen].decode("utf-8")
            offset += plen
            predicates.append((cell_val, p_str))

        # Literals
        lit_count, = struct.unpack_from("<I", data, offset)
        offset += 4
        literals = []
        for _ in range(lit_count):
            cell_val, llen = struct.unpack_from("<QI", data, offset)
            offset += 12
            l_str = data[offset:offset + llen].decode("utf-8")
            offset += llen
            literals.append((cell_val, l_str))

        return cls(slots=slots, predicates=predicates, literals=literals), offset


@dataclass
class InstructionStreamFrame:
    version: int
    namespace: int
    dictionary: StreamDictionary
    local_count: int
    instructions: List[Instruction]

    def encode(self) -> bytes:
        buf = bytearray()
        buf.extend(STREAM_MAGIC)
        buf.extend(struct.pack("<IQ", self.version, self.namespace))
        buf.extend(self.dictionary.encode())
        buf.extend(struct.pack("<II", self.local_count, len(self.instructions)))
        for inst in self.instructions:
            buf.extend(inst.encode())
        return bytes(buf)

    @classmethod
    def decode(cls, data: bytes, offset: int = 0) -> Tuple[InstructionStreamFrame, int]:
        magic = data[offset:offset + 4]
        if magic != STREAM_MAGIC:
            raise ValueError(f"invalid STREAM_MAGIC: {magic}")
        offset += 4

        version, namespace = struct.unpack_from("<IQ", data, offset)
        if version != SEMANTIC_ISA_VERSION_1:
            raise ValueError(f"unsupported ISA version: {version}")
        offset += 12

        dictionary, offset = StreamDictionary.decode(data, offset)

        local_count, inst_count = struct.unpack_from("<II", data, offset)
        offset += 8

        instructions = []
        for _ in range(inst_count):
            inst, offset = Instruction.decode(data, offset)
            instructions.append(inst)

        return cls(
            version=version,
            namespace=namespace,
            dictionary=dictionary,
            local_count=local_count,
            instructions=instructions,
        ), offset


class TransactionBuilder:
    """Domain-agnostic fluent builder for a semantic instruction stream transaction."""

    def __init__(self, compiler: SemanticCompiler) -> None:
        self.compiler = compiler
        self.namespace = compiler._next_namespace()
        self.next_local = 0
        self.slots: Dict[str, int] = {}
        self.slot_list: List[Tuple[int, str]] = []
        self.predicates: Dict[str, int] = {}
        self.pred_list: List[Tuple[int, str]] = []
        self.literals: Dict[str, int] = {}
        self.lit_list: List[Tuple[int, str]] = []
        self.instructions: List[Instruction] = []

    def allocate(self) -> TempEntity:
        temp = TempEntity(namespace=self.namespace, index=self.next_local)
        self.next_local += 1
        self.instructions.append(Instruction(Opcode.ALLOCATE))
        self.instructions.append(Instruction(Opcode.STORE_LOCAL, argument=temp.index))
        return temp

    def _get_slot(self, slot: str) -> int:
        if slot in self.slots:
            return self.slots[slot]
        token = len(self.slots)
        self.slots[slot] = token
        self.slot_list.append((token, slot))
        return token

    def _get_predicate(self, pred: str) -> int:
        if pred in self.predicates:
            return self.predicates[pred]
        cell_val = len(self.predicates)
        self.predicates[pred] = cell_val
        self.pred_list.append((cell_val, pred))
        return cell_val

    def _get_literal(self, lit: str) -> int:
        if lit in self.literals:
            return self.literals[lit]
        cell_val = VM_LITERAL_BASE + len(self.literals)
        self.literals[lit] = cell_val
        self.lit_list.append((cell_val, lit))
        return cell_val

    def _push_atom(self, atom: AtomRef) -> None:
        if isinstance(atom, EntityRef):
            self.instructions.append(Instruction(Opcode.PUSH_CELL, immediate=atom.value))
        elif isinstance(atom, TempEntity):
            self.instructions.append(Instruction(Opcode.LOAD_LOCAL, argument=atom.index))
        elif isinstance(atom, LiteralRef):
            cell_val = self._get_literal(atom.value)
            self.instructions.append(Instruction(Opcode.PUSH_CELL, immediate=cell_val))
        elif isinstance(atom, str):
            cell_val = self._get_literal(atom)
            self.instructions.append(Instruction(Opcode.PUSH_CELL, immediate=cell_val))
        elif isinstance(atom, int):
            self.instructions.append(Instruction(Opcode.PUSH_CELL, immediate=atom))
        else:
            raise TypeError(f"unsupported atom reference type: {type(atom)}")

    def define(self, slot: str, subject: AtomRef, predicate: str, object: AtomRef) -> None:
        token = self._get_slot(slot)
        self._push_atom(subject)
        pred_cell = self._get_predicate(predicate)
        self.instructions.append(Instruction(Opcode.PUSH_CELL, immediate=pred_cell))
        self._push_atom(object)
        self.instructions.append(Instruction(Opcode.DEFINE, argument=token))

    def forget(self, slot: str) -> None:
        token = self._get_slot(slot)
        self.instructions.append(Instruction(Opcode.FORGET, argument=token))

    def expect_object(self, slot: str, expected: AtomRef) -> None:
        token = self._get_slot(slot)
        if isinstance(expected, LiteralRef):
            cell_val = self._get_literal(expected.value)
        elif isinstance(expected, str):
            cell_val = self._get_literal(expected)
        elif isinstance(expected, EntityRef):
            cell_val = expected.value
        elif isinstance(expected, int):
            cell_val = expected
        else:
            raise TypeError(f"unsupported expected reference type: {type(expected)}")
        self.instructions.append(Instruction(Opcode.EXPECT_OBJECT, argument=token, immediate=cell_val))

    def build_frame(self) -> InstructionStreamFrame:
        dictionary = StreamDictionary(
            slots=list(self.slot_list),
            predicates=list(self.pred_list),
            literals=list(self.lit_list),
        )
        return InstructionStreamFrame(
            version=SEMANTIC_ISA_VERSION_1,
            namespace=self.namespace,
            dictionary=dictionary,
            local_count=self.next_local,
            instructions=list(self.instructions),
        )

    def compile_bytes(self) -> bytes:
        return self.build_frame().encode()


class SemanticCompiler:
    """Core domain-agnostic compiler manager for ForthDB Semantic ISA v1."""

    def __init__(self) -> None:
        self._namespace_counter = 1

    def _next_namespace(self) -> int:
        ns = self._namespace_counter
        self._namespace_counter += 1
        return ns

    def transaction(self) -> TransactionBuilder:
        return TransactionBuilder(self)

    @staticmethod
    def encode_frame(frame: InstructionStreamFrame) -> bytes:
        return frame.encode()

    @staticmethod
    def decode_frame(data: bytes, offset: int = 0) -> Tuple[InstructionStreamFrame, int]:
        return InstructionStreamFrame.decode(data, offset)
