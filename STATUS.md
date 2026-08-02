# ForthDB Research Status

**Status date:** August 2, 2026

## Current State

ForthDB has progressed through three executable stages:

1. a semantic kernel
2. an in-memory atomic publication experiment
3. a durable committed-world transaction model

The project’s current implementation direction is the committed-world model in `forthdb_world.py`.

The original kernel remains the semantic reference and should not be discarded or bypassed. The committed-world model constructs and recovers each world by applying operations through that kernel.

## File Status

### Foundation

#### `forthdb_kernel.py`

The canonical semantic reference implementation.

It defines entities, facts, slots, immutable definition records, active heads, history, indexes, query planning, joins, provenance, symbol resolution, compiled identity, and rendering.

Future persistence and transaction experiments should normally use this implementation to interpret `define` and `forget` operations.

#### `library_demo.py`

The baseline regression suite and first application model.

It demonstrates:

- two-hop graph traversal
- redefinition and current-state lookup
- restoration of a previous definition through `forget`
- duplicate assertions with distinct and provenance modes
- efficient current lookup after deep history
- compiled identity surviving display-name changes and symbol rebinding
- a library workflow involving works, copies, shelves, patrons, movement, checkout, and return

This script is the reference semantic evidence.

### Historical experiment

#### `forthdb_atomic.py`

The first executable atomicity model.

It introduced private transaction staging, all-or-nothing in-memory publication, explicit rollback, and failure injection before publication.

It is superseded by `forthdb_world.py` because it does not provide durable commit frames, crash recovery, immutable world identity, or a single committed-world publication object.

It remains in the repository because it records an important step in the project’s reasoning.

#### `atomic_demo.py`

Tests and demonstration for `forthdb_atomic.py`.

This file should remain runnable as preserved evidence of the intermediate model.

### Current model

#### `forthdb_world.py`

The current durable committed-world implementation.

It provides:

- transactions based on one committed world
- private ordered operations
- transaction-local entity allocation
- read-your-own-writes through candidate snapshots
- immutable reader snapshots
- deterministic world digests
- canonical checksummed commit frames
- append and `fsync` before publication
- stale-writer conflict rejection
- application validators
- recovery through the original semantic kernel
- incomplete-tail tolerance
- fail-closed corruption detection

The append log is authoritative. Materialized worlds and indexes are reconstructed state.

#### `world_library_demo.py`

The primary integration, ACID, and recovery evidence.

It runs the library application through the committed-world database and verifies behavior including:

- application results matching the semantic baseline
- snapshot stability across later commits
- durable recovery producing the same version and world digest
- stale writers aborting
- rejected constraints creating no world
- transaction-local reads seeing staged changes
- validators being unable to mutate candidate state
- a transaction surviving failure after `fsync` but before in-memory publication
- incomplete final frames not becoming worlds
- corruption in established history failing closed

## Reproduction Commands

Run from the repository root with a recent Python 3 interpreter:

```bash
python library_demo.py
python atomic_demo.py
python world_library_demo.py
```

The project currently depends only on the Python standard library.

A change should not be considered an improvement if it breaks the baseline kernel behavior or committed-world recovery evidence without an explicit, documented reason.

## Current ACID Claim

The committed-world implementation is an executable ACID model within a single-process ownership boundary.

### Atomicity demonstrated

- staged operations are private
- a transaction is represented by one complete commit frame
- incomplete trailing frames are not applied
- successful publication exposes the complete successor world
- failed candidate construction or validation changes no committed world

### Consistency demonstrated

- candidate worlds run the kernel’s structural validation
- registered constraints may reject candidates before durability
- validators are checked for read-only behavior
- recovery recomputes and verifies the resulting world digest

### Isolation demonstrated

- readers remain pinned to immutable snapshots
- transactions read their own candidate state
- stale writers abort instead of overwriting a later world
- commits are serialized inside one process

### Durability demonstrated

- commit frames are appended and `fsync`ed before in-memory publication
- reopening the database recovers complete committed frames
- a simulated failure after `fsync` but before publication still recovers the transaction
- incomplete tails are ignored
- established-history corruption raises an error rather than being silently repaired

## What Is Not Yet Claimed

The current model is not presented as a production-ready database engine.

It does not yet claim:

- safe concurrent writing by independent processes
- networked or distributed transactions
- slot-level or predicate-level conflict merging
- efficient large-database recovery
- bounded storage growth
- checkpointing or compaction
- persisted indexes
- zero-copy or structurally shared snapshots
- optimized commit latency or throughput
- a stable public file-format compatibility guarantee

These are implementation, optimization, and productization questions. They are not currently reasons to change the committed-world semantic contract.

## Multiple Writers

The current design decision is complete enough for now:

> A transaction extends exactly the committed world on which it was based. If a later world has already been published, the stale transaction must not silently commit over it.

The current implementation enforces this among writers inside one process.

Coordination among independent processes is deferred. It may later use file locking, a single writer service, a server coordinator, compare-and-swap storage, or another mechanism. Choosing that mechanism is an implementation problem and should not force a premature change to the transaction model.

## Decisions to Preserve

The following decisions represent the current research baseline:

1. `forthdb_kernel.py` remains the canonical semantic implementation.
2. Later models should apply operations through the kernel instead of casually duplicating its behavior.
3. The library application is a continuing compatibility test, not a disposable demo.
4. The append log of complete valid commit frames is authoritative.
5. A transaction creates an immutable successor world rather than mutating the current one.
6. Readers capture one immutable world.
7. A stale transaction aborts unless a future explicit conflict-resolution model says otherwise.
8. Indexes and active-head materializations are derived state.
9. Durability precedes in-memory publication and reported commit success.
10. Performance work should follow evidence, not anticipation.

## Current Research Questions

These questions remain open but are not immediate blockers:

- What should a stable long-term commit-frame format contain?
- Should definition identity eventually be independent of reference-kernel list position?
- What checkpoint form preserves deterministic world identity?
- What retention contracts should govern compaction and archival?
- Which multi-process commit coordinator best fits the committed-world model?
- When is coarse stale-world rejection too restrictive for real applications?
- Which application should follow the library model to test a substantially different workload?

## Immediate Maintenance Priorities

The near-term priority is preservation rather than expansion:

- keep all three scripts runnable
- document changes with descriptive commit messages
- add automated execution of the regression scripts
- keep architecture and status documents synchronized with demonstrated behavior
- avoid performance claims not supported by measurement
- avoid broadening the ACID claim beyond the tested concurrency boundary

## Milestone Summary

The project has moved beyond isolated database primitives.

It now has:

- a stable semantic reference
- an application-level regression model
- an executable explanation of atomic publication
- a durable committed-world architecture
- a bounded ACID contract
- deterministic recovery evidence
- a public repository capable of transmitting the research state between independent sessions

The next implementation work should deepen or challenge this model without discarding the evidence that produced it.
