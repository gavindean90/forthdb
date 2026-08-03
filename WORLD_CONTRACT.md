# ForthDB World Contract

## Purpose

This document defines the semantic contract of ForthDB's committed-world model. It specifies meaning rather than storage technology.

## Core Principle

The authoritative state of a ForthDB database is an immutable sequence of valid worlds. Operations construct private candidate successors; they never mutate an existing world.

## World

A World is the complete immutable logical state visible to readers. It contains active slot definitions, allocator state, version information, and other authoritative kernel state. Derived indexes and caches are implementation details.

## Strict Transaction

A strict `Transaction` has exactly one absolute base World and an ordered sequence of operations. Commit succeeds only when that base remains the current World. Otherwise stale-writer rejection leaves history unchanged.

## Queued Intent

A `QueuedIntent` explicitly delegates predecessor assignment to an ordered epoch planner. It is not an automatically rebased strict transaction.

Queued-intent preconditions are evaluated against the private predecessor assigned by the planner. Temporary entity handles are scoped to one intent and become permanent identifiers only when that intent is successfully derived from its predecessor.

A rejected intent produces no World or Commit Frame and consumes neither version nor allocator state.

## Bounded Admission and Tickets

Queued ingress is bounded before candidate derivation. A saturated ingress rejects the submission immediately and returns ownership of the unadmitted intent.

Successful admission creates a `CommitTicket` with an observable queued, claimed, and resolved lifecycle. Ticket lifetime does not control intent lifetime.

Dropping or otherwise abandoning a ticket:

- does not remove an admitted intent from ingress
- does not cancel a claimed intent
- does not change epoch order
- does not alter canonical history

After the authoritative transition completes, result delivery may fail because the caller abandoned its ticket. That is a notification failure, not a commit rollback.

## Candidate World

A Candidate World is private during derivation and validation. It becomes a committed World only after its Commit Frame satisfies the active publication and durability contract; otherwise it is discarded.

## Validation

Validation is deterministic and checks kernel invariants plus application-defined invariants. Validation never mutates the Candidate World.

## Commit Frame

A Commit Frame is the durable description of one successful transition from World N to World N+1. It is immutable and remains the smallest durable history unit.

A durability epoch may synchronize several independent Commit Frames with one barrier. The epoch does not turn those frames into one compound transaction.

## Publication

Publication is atomic. A strict commit advances the reader head to its successor. A queued epoch may advance the reader head once from the pre-epoch World to the epoch tail while retaining every accepted intermediate World in canonical history and returning each accepted caller its own immutable World.

No success result may be delivered before the required publication and durability conditions are satisfied.

## Commit Store

A Commit Store records Commit Frames. Storage technology—memory, ordinary files, mmap, io_uring, or later epoch transports—is an implementation concern.

A fallible epoch store must publish nothing after an observed epoch failure. It may accept later writes only after rollback has been synchronized and verified against recovery. If repair cannot be verified, the store must enter a poisoned state and reject subsequent writes.

A poisoned physical store does not retroactively invalidate immutable Worlds and Commit Frames that were verified and published before the failure. It must fence later writes and any operation that claims to inspect or extend the uncertain physical tail.

## Recovery

Recovery reconstructs the newest valid World from committed history, ignoring incomplete frames and never inventing state.

Unless a future format adds an explicit atomic epoch envelope, crash recovery may expose the longest sound Commit Frame prefix of an interrupted epoch. That is truthful prefix recovery, not all-or-none epoch recovery.

A complete malformed frame, checksum mismatch, or nonlinear history is corruption and must not be silently discarded as an incomplete tail.

## Derived Structures

Indexes, caches, compiled queries, persistent-node topology, reaper metadata, ingress accounting, ticket metadata, epoch arenas, repair checkpoints, and storage-specific directories are derived state and must remain reconstructible from committed history.

## Historical Truth

History is authoritative. Rollback creates a new World rather than erasing prior Worlds.

## Implemented Milestones

1. `MemoryCommitStore`
2. `FileCommitStore`
3. `MmapCommitStore`
4. `IoUringCommitStore`
5. Structurally shared worlds and background semantic-kernel retirement
6. Queued-intent semantic epoch control
7. Bounded ingress and commit-ticket lifecycle
8. Ordinary-file durability epochs with verified repair and poisoning

Detailed Milestone 6 contracts:

- [`rust/QUEUED_DURABILITY.md`](rust/QUEUED_DURABILITY.md)
- [`rust/FILE_EPOCHS.md`](rust/FILE_EPOCHS.md)
