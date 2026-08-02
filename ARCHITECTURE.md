# ForthDB Architecture

## Purpose

This document records the current architectural contract of ForthDB.

The project began as a small semantic kernel. That kernel remains the reference implementation of what ForthDB operations mean. The committed-world model adds transactions, persistence, recovery, and an ACID contract without replacing or independently reimplementing those semantics.

The central idea is:

> **ForthDB is an append-only chain of complete, validated commit frames. Each frame deterministically produces one immutable world from its parent. A transaction creates the next world rather than mutating the current one.**

## Architectural Layers

### 1. Semantic kernel

`forthdb_kernel.py` defines the meaning of:

- `EntityId`
- `SlotId`
- `RecordId`
- `Fact`
- `Pattern`
- `define`
- `forget`
- active definitions
- definition history
- indexes
- query execution
- symbol binding and compilation
- display-name rendering

This file is the canonical semantic reference. Storage and transaction models should apply operations through it whenever possible.

### 2. Baseline application

`library_demo.py` exercises the kernel as an application rather than merely as a collection of isolated unit operations.

It establishes reference behavior for:

- graph traversal
- redefinition of a slot
- restoration of a prior definition through `forget`
- duplicate facts and provenance
- deep definition history with a one-record current lookup
- stable compiled identity across renaming and symbol rebinding
- a small library domain with works, copies, shelves, patrons, checkout, movement, and return

This application is the semantic baseline that later models must preserve.

### 3. Atomic publication experiment

`forthdb_atomic.py` and `atomic_demo.py` model private staging and all-or-nothing in-memory publication.

They were an important intermediate step, but they still treated publication as replacement of several mutable facade components. They do not claim crash durability or immutable committed-world identity.

They are retained as research history and regression evidence, but they are superseded by the committed-world model.

### 4. Committed-world model

`forthdb_world.py` wraps the semantic kernel in a durable transaction model.

`world_library_demo.py` runs the library workload through that model and tests the transaction, snapshot, recovery, and corruption contracts.

## Core Objects

### Definition

A definition is an immutable fact associated with a stable slot.

A slot has one active definition in a given world. Redefining the slot creates another definition while preserving the prior one. Forgetting a slot changes which earlier definition is active in the successor world.

The current reference kernel materializes definitions as immutable records in an append-only logical log.

### Operation

A transaction contains an ordered sequence of operations:

- `DefineOp(slot, fact)`
- `ForgetOp(slot)`

Operation order is significant. A candidate world is produced by applying the complete ordered sequence through the reference kernel.

### World

A committed world is one immutable, queryable interpretation of the database.

The current materialized world contains:

- a monotonically increasing version
- a deterministic world digest
- a private kernel instance containing the world’s records, active heads, entity allocator state, and derived indexes

Once a world is published, that kernel instance is not mutated. Readers may retain the world safely while later worlds are committed.

### Transaction

A transaction is a private candidate successor to exactly one committed world.

It contains:

- its base world
- ordered staged operations
- transaction-local entity allocation state
- optional read-only validators

The transaction may query its candidate state before commit. Those reads see the base world plus the staged operations, while public readers continue to see the last committed world.

### Commit frame

A durable transaction is represented by one complete append frame.

The frame contains a canonical payload with information including:

- format version
- commit version
- parent-world digest
- ordered operations
- resulting entity allocator state
- resulting-world digest

The payload is framed with a magic value, length information, and a SHA-256 checksum.

A complete valid frame creates a world. An incomplete frame does not.

## Authoritative and Derived State

### Authoritative state

The append log of complete valid commit frames is the durable database.

The durable current database is the world produced by the last complete valid frame in the chain.

### Derived state

The following are materialized accelerators and may be rebuilt:

- the active slot-head map
- subject, predicate, object, pair, and exact indexes
- query executors
- cached world objects
- future checkpoints or persisted index structures

Indexes are not independently authoritative. Recovery may reconstruct them by replaying the committed history through the semantic kernel.

## Commit Protocol

The current commit protocol is deliberately conservative:

1. Capture immutable base World N.
2. Stage operations privately.
3. Clone the base semantic state into a candidate.
4. Apply all operations through the original kernel.
5. Validate kernel invariants.
6. Run registered application validators.
7. Verify that validators did not mutate the candidate.
8. Enter the in-process commit gate.
9. Confirm that World N is still current.
10. Serialize one canonical commit frame for World N+1.
11. Append the complete frame.
12. Flush and `fsync` the append file.
13. Publish one new in-memory world reference.
14. Return commit success.

The ordering is intentional:

```text
make durable → publish in memory → report success
```

A process failure after durable append but before in-memory publication is recoverable: reopening the database discovers the valid frame and reconstructs the committed world.

## ACID Contract

### Atomicity

The commit frame is the unit of atomicity.

A transaction is either represented by one complete valid frame or it is not part of the database. An incomplete trailing frame is ignored during recovery. There is no partially visible transaction and no undo procedure for an uncommitted candidate world.

### Consistency

The state-transition rule is:

```text
apply(valid world, valid commit) → valid world
```

A candidate must satisfy the kernel invariants and all registered application constraints before the frame is appended. A failed candidate produces no committed successor world.

Validators are required to be read-only. The model checks the candidate digest before and after validation and rejects mutation by a validator.

### Isolation

A query captures one immutable committed world and uses it throughout the operation.

A transaction names its base world. At the commit gate, the transaction may publish only if that base is still current. If another writer has already committed a successor, the stale transaction aborts with a conflict rather than losing or silently overwriting the intervening work.

This is deliberately conservative. It provides a clear serial commit order and stable reader snapshots without row locks, predicate locks, undo records, deadlock handling, or mutable shared pages.

### Durability

Commit success is returned only after the complete frame has been appended and `fsync` has completed.

Recovery verifies frame structure, lengths, checksums, parent continuity, and resulting world digests. It reconstructs complete valid commits in order.

An incomplete final frame is treated as an uncommitted torn tail. Corruption within established committed history fails closed.

## Recovery

Recovery begins from the genesis world and scans frames in append order.

For each complete frame, recovery:

1. verifies framing and checksum
2. decodes the canonical payload
3. verifies the expected version and parent digest
4. applies the ordered operations through `forthdb_kernel.py`
5. restores the committed entity allocator state
6. validates the reconstructed kernel
7. recomputes and verifies the resulting-world digest
8. materializes the next immutable committed world

The last complete valid reconstructed world becomes current.

## Entity Allocation

Entity allocation participates in the transaction.

`WorldTransaction.entity()` allocates from a private transaction-local high-water mark. The resulting allocator state becomes authoritative only if the transaction commits.

This prevents uncommitted entity allocation from changing the public world and lets recovery restore the allocator deterministically from committed history.

The current model uses integer entity identities inherited from the reference kernel. More elaborate identity allocation may be explored later without changing the committed-world contract.

## Multiple Writers

The design contract and implementation mechanism are intentionally separated.

The design contract is:

> A transaction may extend only the committed world on which it was based. It must not publish over a later world without an explicit conflict-resolution model.

The current implementation serializes commits with an in-process lock and rejects stale transactions.

It does not yet coordinate independent processes writing the same file. Possible later mechanisms include:

- operating-system file locks
- one dedicated writer process
- a database server commit coordinator
- compare-and-swap publication on shared storage
- finer slot-level conflict detection

These are implementation choices. They do not change the meaning of a transaction, commit, world, or stale-writer conflict.

## History, Checkpoints, and Compaction

The prototype retains complete committed history because this is the simplest way to make semantics and recovery unambiguous.

The architecture does not require every historical frame to remain forever in one online file.

Possible later storage policies include:

- periodic checkpoints
- bounded history retention
- archival of older commits
- compaction before an accepted checkpoint
- persisted derived indexes
- structural sharing between materialized worlds

Any such optimization must preserve the documented retention guarantees and reproduce the same current world as replaying the retained authoritative history.

Compaction is therefore a background storage-engine concern, not an intrinsic change to the semantic kernel or committed-world transaction contract.

## Current Boundaries

The current model intentionally does not optimize:

- candidate-world cloning
- recovery time from genesis
- commit-frame size
- index persistence
- memory sharing between worlds
- write concurrency
- cross-process coordination
- compaction
- checkpoint selection

These are not treated as defects in the design unless an application or implementation experiment produces evidence that changes the semantic contract.

## Preservation Rule

Future models should satisfy two conditions:

1. They should interpret ForthDB operations through the reference kernel, or explicitly demonstrate semantic equivalence to it.
2. They should reproduce the baseline library behavior and the committed-world recovery tests before replacing the current model.

This keeps the research cumulative. New implementation work may improve how worlds are stored or constructed without quietly changing what a ForthDB world means.
