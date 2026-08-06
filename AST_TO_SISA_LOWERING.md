# AST to SISA Lowering

Source model: Transaction AST v0
Target model: SISA v1
Lowering revision: 1

This document specifies the exact bytes emitted by the canonical lowering pass from `TransactionAST` to `SISA v1`.

## Types and Tags

- `SlotRef`: Represented as a String. Mapped to a unique `u32` frame-local slot token during lowering.
- `PredicateRef`: Represented as a String. Mapped to a unique `u64` frame-local predicate cell.
- `LiteralRef`: Represented as a String. Mapped to a unique `u64` frame-local literal cell.
- `EntityId`: Unsigned 64-bit integer, emitted natively as `u64`.
- `Symbol`: Identified by a unique string. Mapped to a `u32` frame-local register index.

## Lowering Rules

### Pre-Allocation Pass (Dictionary Builder)
Before emitting instructions, the compiler collects all unique names for `SlotRef`, `PredicateRef`, and `LiteralRef` into three ordered dictionaries.

1. **Slots**: Sort alphabetically. Their 0-indexed position becomes their `slot_token` (`u32`).
2. **Predicates**: Sort alphabetically. Their 0-indexed position becomes their `predicate_token` (`u64`).
3. **Literals**: Sort alphabetically. Their 0-indexed position becomes their `literal_token` (`u64`), offset by the number of predicates (`literal_cell = predicate_count + literal_token`).

All dictionary strings are appended into the SISA frame header.

### Operations

#### `Allocate { result: Symbol }`
Emits an `Allocate` opcode, then stores the returned cell into the local register assigned to `Symbol`.
```text
Allocate
StoreLocal(register_index)
```

#### `ExpectWorld { expected: WorldId }`
Asserts that the current world matches the exact predecessor state.
```text
ExpectObject(u32::MAX, world_id)
```

#### `ExpectObject { slot: SlotRef, expected: AtomRef }`
Resolves the expected value (either pushing a cell for a literal/entity or loading from a local register).
If `AtomRef` is an `EntityId` or `LiteralRef`, its cell value is used directly as the immediate.
```text
ExpectObject(slot_token, expected_cell)
```

#### `Define { slot: SlotRef, subject: AtomRef, predicate: PredicateRef, object: AtomRef }`
Pushes the subject, predicate, and object onto the execution stack.
For `AtomRef`, if it is an `Entity` or `Literal`, the compiler uses `PushCell`.
If it is a `Symbol`, it uses `LoadLocal`.
```text
(PushCell | LoadLocal)(subject)
(PushCell | LoadLocal)(predicate)
(PushCell | LoadLocal)(object)
Define(slot_token)
```

#### `Forget { slot: SlotRef }`
```text
Forget(slot_token)
```

#### `Reject`
```text
Reject
```

## Golden Vectors

*(Detailed bytes will be implemented and checked in the respective test suites)*
