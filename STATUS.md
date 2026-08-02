# ForthDB Research Status

**Status date:** August 2, 2026

## Current State

ForthDB has progressed through three executable stages:

1. a semantic kernel
2. an in-memory atomic publication experiment
3. a durable committed-world transaction model

The project’s current implementation direction is the committed-world model in `forthdb_world.py`.

The original kernel remains the semantic reference and should not be discarded or bypassed. The committed-world model constructs and recovers each world by applying operations through that kernel.

The repository now also contains a unified research regression and a GitHub Actions workflow. Together they make the public repository self-verifying: a clean machine can run the existing evidence, compare the original and durable application models, and publish a structured report without relying on conversation context.

## File Status

### Foundation

#### `forthdb_kernel.py`

The canonical semantic reference implementation.

It defines entities, facts, slots, immutable definition records, active heads, history, indexes, query planning, joins, provenance, symbol resolution, compiled identity, and rendering.

Future persistence and transaction experiments should normally use this implementation to interpret `define` and `forget` operations.

#### `library_demo.py`

The baseline regression suite and first application model.

It demonstrates graph traversal, redefinition, `forget`, duplicate assertions and provenance, deep history with one-record current lookup, compiled identity, and a small library workflow.

This script is the reference semantic evidence.

### Historical experiment

#### `forthdb_atomic.py`

The first executable atomicity model.

It introduced private transaction staging, all-or-nothing in-memory publication, explicit rollback, and failure injection before publication.

It is superseded by `forthdb_world.py` because it does not provide durable commit frames, crash recovery, immutable world identity, or a single committed-world publication object.

It remains in the repository because it records an important step in the project’s reasoning.

#### `atomic_demo.py`

Tests and demonstration for `forthdb_atomic.py`.

This file remains runnable as preserved evidence of the intermediate model.

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

The primary component-level integration, ACID, and recovery evidence.

It runs the library application through the committed-world database and verifies snapshot stability, stale-writer rejection, constraint rejection, transaction-local reads, validator immutability, recovery after failure between `fsync` and publication, incomplete-tail handling, and fail-closed corruption detection.

### Preservation infrastructure

#### `research_regression.py`

The primary unified research regression.

It:

- runs the semantic kernel suite
- runs the historical atomic suite
- runs the committed-world ACID suite
- executes the original library model
- executes the committed-world library model in two separate Python processes
- uses different `PYTHONHASHSEED` values for those processes
- compares a shared semantic projection across the original and durable models
- verifies restart recovery against the live committed world
- verifies deterministic world identity
- verifies byte-identical durable commit logs
- emits JSON and Markdown evidence reports

The semantic projection is a contract check. Observations such as record counts, frame size, and final version are reported but are not automatically frozen as permanent architectural requirements.

#### `.github/workflows/research-regression.yml`

The clean-machine witness for the research regression.

It runs on pushes to `main`, pull requests, and manual dispatches using a reference Python 3.13 environment. It compiles the sources, runs `research_regression.py`, publishes the Markdown report to the job summary, uploads both reports as an artifact, and verifies that execution leaves the checkout unchanged.

The workflow is intentionally thin. Correctness remains defined by executable Python assertions that can also run locally.

## Reproduction Commands

Run the complete research regression from the repository root:

```bash
python research_regression.py
```

The reports are written to `artifacts/` by default.

The individual stages remain available:

```bash
python library_demo.py
python atomic_demo.py
python world_library_demo.py
```

The project currently depends only on the Python standard library.

A change should not be considered an improvement if it breaks the baseline kernel behavior, cross-model semantic continuity, or committed-world recovery evidence without an explicit and documented reason.

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
11. Research assertions belong in locally runnable code; GitHub Actions supplies an independent execution environment and preserved run evidence.

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

The near-term priority remains preservation rather than expansion:

- keep the unified research regression green
- keep all three component scripts independently runnable
- keep architecture and status documents synchronized with demonstrated behavior
- preserve cross-model semantic continuity
- treat generated metrics as observations unless deliberately promoted to contracts
- avoid performance claims not supported by measurement
- avoid broadening the ACID claim beyond the tested concurrency boundary

## Milestone Summary

The project now has:

- a stable semantic reference
- an application-level regression model
- an executable explanation of atomic publication
- a durable committed-world architecture
- a bounded ACID contract
- deterministic recovery evidence
- a cross-model semantic compatibility test
- deterministic durable-byte evidence across separate processes
- a clean-machine GitHub Actions witness with preserved JSON and Markdown reports
- a public repository capable of transmitting and independently verifying the research state

The next implementation work should deepen or challenge this model without discarding the evidence that produced it.
