# Phase 1 Stack VM Experiment

## Question

Does replacing per-intent `ForthDb` clones with a framed stack and POD arena
remove the wide-epoch materialization collapse?

This experiment deliberately leaves the durable admission journal, io_uring
transport, recovery decoder, and production world implementation unchanged.
It is an in-memory materialization kernel, not a second durable engine.

## Experimental machine

Each intent is a relocatable `IntentProgram` containing fixed-size
instructions. A preallocated `Cell` stack provides frame-relative locals for
temporary entities. Defines and forgets append plain `Copy` records and slot
deltas to logical arenas whose active frontier can be restored without dropping
heap allocations.

Trial deltas remain private until the intent is accepted. Reads consult the
current trial tail before the accepted slot-head table. Rejection restores the
stack, record and delta frontiers plus the entity allocator. Acceptance applies
the trial's slot-head deltas once.

`FORGET` records retain both the previous visible definition and the resulting
revealed definition, preserving ForthDB's existing unmasking behavior.

## Differential contract

The checked-in tests compare the stack VM with `derive_epoch_world` across
epoch widths 16, 64, 128, and 256. The trace includes:

- frame-local temporary allocation
- repeated definitions of the same slots
- post-write validator rejection
- allocator rollback after rejection
- `FORGET` revealing an older definition
- accepted/rejected outcome parity
- visible slot-value, active-slot, and allocator-frontier parity

## Benchmark scope

The `stack_vm_phase1` benchmark measures the current materializer and the POD
kernel over the same intent counts and rejection schedule. It reports a
**projection-kernel ratio**, not an end-to-end database speedup.

The experimental VM does not yet provide:

- subject, predicate, object, or compound query indexes
- immutable `World` and `CommitFrame` construction
- deterministic world hashing
- arbitrary host-closure validators
- durable token encoding or recovery
- concurrent readers or published world roots

A large result therefore establishes that cloning and heap-backed state are
material costs and that the proposed kernel has sufficient CPU headroom. It
does not establish production semantic-publication throughput. The next phase
must add index-delta layers and immutable world roots while preserving the
differential contract.
