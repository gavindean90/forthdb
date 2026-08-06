import struct
from typing import List, Union

class AtomRef:
    pass

class EntityId(AtomRef):
    def __init__(self, value: int):
        self.value = value
    def __eq__(self, other):
        return isinstance(other, EntityId) and self.value == other.value

class LiteralRef(AtomRef):
    def __init__(self, value: str):
        self.value = value
    def __eq__(self, other):
        return isinstance(other, LiteralRef) and self.value == other.value

class SymbolRef(AtomRef):
    def __init__(self, name: str):
        self.name = name
    def __eq__(self, other):
        return isinstance(other, SymbolRef) and self.name == other.name

class WorldId:
    def __init__(self, value: int):
        self.value = value
    def __eq__(self, other):
        return isinstance(other, WorldId) and self.value == other.value

class TransactionOp:
    pass

class AllocateOp(TransactionOp):
    def __init__(self, result: str):
        self.result = result

class ExpectWorldOp(TransactionOp):
    def __init__(self, expected: WorldId):
        self.expected = expected

class ExpectObjectOp(TransactionOp):
    def __init__(self, slot: str, expected: AtomRef):
        self.slot = slot
        self.expected = expected

class DefineOp(TransactionOp):
    def __init__(self, slot: str, subject: AtomRef, predicate: str, object: AtomRef):
        self.slot = slot
        self.subject = subject
        self.predicate = predicate
        self.object = object

class ForgetOp(TransactionOp):
    def __init__(self, slot: str):
        self.slot = slot

class RejectOp(TransactionOp):
    pass

class TransactionAST:
    def __init__(self, namespace: int, operations: List[TransactionOp]):
        self.namespace = namespace
        self.operations = operations

    def lower_to_sisa(self) -> bytes:
        slots = set()
        predicates = set()
        literals = set()
        symbols = {}
        local_count = 0

        for op in self.operations:
            if isinstance(op, AllocateOp):
                if op.result in symbols:
                    raise ValueError(f"duplicate allocation for symbol '{op.result}'")
                symbols[op.result] = local_count
                local_count += 1
            elif isinstance(op, ExpectWorldOp):
                pass
            elif isinstance(op, ExpectObjectOp):
                slots.add(op.slot)
                if isinstance(op.expected, LiteralRef):
                    literals.add(op.expected.value)
            elif isinstance(op, DefineOp):
                slots.add(op.slot)
                predicates.add(op.predicate)
                if isinstance(op.subject, LiteralRef):
                    literals.add(op.subject.value)
                if isinstance(op.object, LiteralRef):
                    literals.add(op.object.value)
            elif isinstance(op, ForgetOp):
                slots.add(op.slot)
            elif isinstance(op, RejectOp):
                pass
        
        sorted_slots = sorted(list(slots))
        slot_map = {s: i for i, s in enumerate(sorted_slots)}

        sorted_predicates = sorted(list(predicates))
        predicate_map = {p: i for i, p in enumerate(sorted_predicates)}

        sorted_literals = sorted(list(literals))
        literal_map = {l: i + len(predicate_map) for i, l in enumerate(sorted_literals)}

        out = bytearray()
        
        # Header
        out.extend(struct.pack("<I", 1)) # version 1
        out.extend(struct.pack("<Q", self.namespace))
        out.extend(struct.pack("<I", len(sorted_slots)))
        out.extend(struct.pack("<I", len(sorted_predicates)))
        out.extend(struct.pack("<I", len(sorted_literals)))
        out.extend(struct.pack("<I", local_count))
        
        for slot in sorted_slots:
            encoded = slot.encode('utf-8')
            out.extend(struct.pack("<I", len(encoded)))
            out.extend(encoded)
            
        for predicate in sorted_predicates:
            encoded = predicate.encode('utf-8')
            out.extend(struct.pack("<I", len(encoded)))
            out.extend(encoded)
            
        for literal in sorted_literals:
            encoded = literal.encode('utf-8')
            out.extend(struct.pack("<I", len(encoded)))
            out.extend(encoded)

        instructions = bytearray()
        
        def load_atom(atom: AtomRef):
            if isinstance(atom, EntityId):
                instructions.extend(struct.pack("<BBHL", 4, 0, 0, 0)) # PushCell (Op 4)
                instructions.extend(struct.pack("<Q", atom.value))
            elif isinstance(atom, LiteralRef):
                cell = literal_map[atom.value]
                instructions.extend(struct.pack("<BBHL", 4, 0, 0, 0)) # PushCell
                instructions.extend(struct.pack("<Q", cell))
            elif isinstance(atom, SymbolRef):
                if atom.name not in symbols:
                    raise ValueError(f"use of undefined symbol '{atom.name}'")
                local = symbols[atom.name]
                instructions.extend(struct.pack("<BBHL", 2, 0, 0, local)) # LoadLocal (Op 2)
                instructions.extend(struct.pack("<Q", 0))
            else:
                raise ValueError("Invalid AtomRef")

        for op in self.operations:
            if isinstance(op, AllocateOp):
                local = symbols[op.result]
                instructions.extend(struct.pack("<BBHL", 0, 0, 0, 0)) # Allocate (Op 0)
                instructions.extend(struct.pack("<Q", 0))
                instructions.extend(struct.pack("<BBHL", 3, 0, 0, local)) # StoreLocal (Op 3)
                instructions.extend(struct.pack("<Q", 0))
            elif isinstance(op, ExpectWorldOp):
                instructions.extend(struct.pack("<BBHL", 6, 0, 0, 0xFFFFFFFF)) # ExpectObject (Op 6)
                instructions.extend(struct.pack("<Q", op.expected.value))
            elif isinstance(op, ExpectObjectOp):
                token = slot_map[op.slot]
                if isinstance(op.expected, EntityId):
                    cell = op.expected.value
                elif isinstance(op.expected, LiteralRef):
                    cell = literal_map[op.expected.value]
                elif isinstance(op.expected, SymbolRef):
                    raise ValueError("ExpectObject cannot take a dynamic symbol as expected value in SISA v1")
                instructions.extend(struct.pack("<BBHL", 6, 0, 0, token)) # ExpectObject
                instructions.extend(struct.pack("<Q", cell))
            elif isinstance(op, DefineOp):
                load_atom(op.subject)
                
                pred_cell = predicate_map[op.predicate]
                instructions.extend(struct.pack("<BBHL", 4, 0, 0, 0)) # PushCell
                instructions.extend(struct.pack("<Q", pred_cell))
                
                load_atom(op.object)
                
                token = slot_map[op.slot]
                instructions.extend(struct.pack("<BBHL", 5, 0, 0, token)) # Define (Op 5)
                instructions.extend(struct.pack("<Q", 0))
            elif isinstance(op, ForgetOp):
                token = slot_map[op.slot]
                instructions.extend(struct.pack("<BBHL", 7, 0, 0, token)) # Forget (Op 7)
                instructions.extend(struct.pack("<Q", 0))
            elif isinstance(op, RejectOp):
                instructions.extend(struct.pack("<BBHL", 8, 0, 0, 0)) # Reject (Op 8)
                instructions.extend(struct.pack("<Q", 0))
                
        out.extend(struct.pack("<I", len(instructions) // 16))
        out.extend(instructions)
        return bytes(out)
