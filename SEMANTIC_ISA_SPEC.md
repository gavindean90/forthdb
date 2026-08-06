# ForthDB Semantic ISA v1 Specification

**Version**: `1`  
**Magic Bytes**: `b"SISA"`  
**Status**: Stable Public Execution Contract  

This document specifies the binary encoding and execution contract for **Semantic ISA v1**, ForthDB's stable public interface between client frontends (Python, C#, Rust, Forth) and the Semantic Kernel.

---

## 1. High-Level Architecture

```text
Frontend (Python, C#, Rust, native Forth, etc.)
                    ↓
        Semantic Instruction Stream (v1 Binary)
                    ↓
              Semantic Kernel
                    ↓
         Storage / Durability / Indexes
```

The Semantic Instruction Stream decouples language-specific frontends from internal database storage layout. Clients compile high-level domain operations into self-contained binary frames containing opcode instructions and stream dictionary preambles.

---

## 2. Binary Framing (`InstructionStreamFrame`)

A `b"SISA"` stream frame consists of three major sections serialized in Little-Endian byte order:

```text
+-----------------------+-----------------------+-----------------------+
|  Stream Header        |  Stream Dictionary    |  Execution Body       |
|  (Magic, Ver, NS)     |  (Slots, Pred, Lit)   |  (Locals, Instructions)|
+-----------------------+-----------------------+-----------------------+
```

### 2.1 Stream Header (16 bytes)

| Offset | Field | Type | Description |
| :---: | :--- | :--- | :--- |
| `0..4` | `magic` | `[u8; 4]` | ASCII byte string `b"SISA"` (`0x53 0x49 0x53 0x41`) |
| `4..8` | `version` | `u32` (LE) | Format version, currently `1` |
| `8..16` | `namespace` | `u64` (LE) | Intent/Transaction namespace identifier |

---

### 2.2 Stream Dictionary Preamble

The preamble contains localized string-to-token mappings required to execute the frame without polling global state.

#### 1. Slot Definitions
- `count`: `u32` (LE)
- Array of entries:
  - `token`: `u32` (LE) — Local slot token ID
  - `length`: `u32` (LE) — Byte length of string
  - `name`: `[u8; length]` — UTF-8 encoded slot name string

#### 2. Predicate Definitions
- `count`: `u32` (LE)
- Array of entries:
  - `cell`: `u64` (LE) — Predicate cell ID
  - `length`: `u32` (LE) — Byte length of string
  - `name`: `[u8; length]` — UTF-8 encoded predicate name string

#### 3. Literal Definitions
- `count`: `u32` (LE)
- Array of entries:
  - `cell`: `u64` (LE) — Literal cell ID (typically offset from `VM_LITERAL_BASE = 1 << 63`)
  - `length`: `u32` (LE) — Byte length of string
  - `value`: `[u8; length]` — UTF-8 encoded string literal value

---

### 2.3 Execution Body

- `local_count`: `u32` (LE) — Number of temporary register slots required by frame
- `inst_count`: `u32` (LE) — Total number of instructions
- `instructions`: Array of 13-byte fixed-width instruction encodings.

---

## 3. Instruction Encoding (13 Bytes Fixed)

Each instruction is encoded as a fixed 13-byte record:

```text
+-------------------+-------------------+-------------------+
|  Opcode (1 byte)  |  Argument (4B)    |  Immediate (8B)   |
+-------------------+-------------------+-------------------+
```

| Offset | Field | Type | Description |
| :---: | :--- | :--- | :--- |
| `0..1` | `opcode` | `u8` | Discriminator (0 to 8) |
| `1..5` | `argument` | `u32` (LE) | Register index or slot token ID |
| `5..13` | `immediate` | `u64` (LE) | Literal cell ID, entity cell ID, or flag |

### 3.1 Opcode Discriminators

| Code | Opcode Name | Argument (`u32`) | Immediate (`u64`) | Semantics |
| :---: | :--- | :--- | :--- | :--- |
| `0` | `ExpectObject` | `slot_token` / `u32::MAX` | `expected_cell` / `world_id` | Asserts slot value or expected predecessor world |
| `1` | `Allocate` | `0` | `0` | Allocates next sequential entity ID and pushes to stack |
| `2` | `AllocateDiscard` | `0` | `0` | Advances entity allocator without storing reference |
| `3` | `LoadLocal` | `local_idx` | `0` | Pushes value from local register `local_idx` onto stack |
| `4` | `StoreLocal` | `local_idx` | `0` | Pops value from stack and stores in local register `local_idx` |
| `5` | `PushCell` | `0` | `cell_value` | Pushes literal or entity cell ID onto stack |
| `6` | `Define` | `slot_token` | `0` | Pops `object`, `predicate`, `subject` and defines slot |
| `7` | `Forget` | `slot_token` | `0` | Retracts current fact definition in slot |
| `8` | `Reject` | `0` | `0` | Aborts transaction execution and rolls back trial frame |

---

## 4. Stack-VM Execution Contract

1. **Self-Contained Resolution**: Opcode evaluation resolves string names via preamble dictionaries without touching external network or global symbol tables.
2. **Deterministic Trial Scoping**: Local registers (`LoadLocal`/`StoreLocal`) isolate temporary handles within the transaction boundary.
3. **Atomicity & Parity**: Trial frames maintain transactional copy-on-write integrity; any `ExpectObject` failure or `Reject` opcode triggers complete trial rollback.
