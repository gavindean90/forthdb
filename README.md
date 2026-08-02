# forthdb

> **Current milestone:** Preserve the original Python kernel as the semantic reference implementation. Build transactional, durable, and future storage models by running their operations through that kernel. Require every model to reproduce the established application behavior before it is accepted as progress.

ForthDB is an experimental database project built from a small set of composable ideas:

- stable entity identities
- immutable definitions
- named slots with one active definition
- reversible definition history
- indexed fact queries and joins
- human-readable symbols compiled to stable identities

The project began with a simple question:

> *If we forgot what a database is supposed to look like and started from a very small set of ideas, what would naturally emerge?*

The current answer is taking the shape of an append-oriented key-value and fact database in which a transaction creates a complete new immutable world rather than mutating the existing one.

## Where the Project Is Now

The original kernel remains the canonical definition of ForthDB semantics. It establishes what entities, facts, slots, definitions, `define`, `forget`, symbol compilation, indexes, and queries mean.

The current durable model wraps those semantics in a committed-world architecture:

1. A transaction captures one committed world.
2. Its operations are staged privately.
3. The operations are applied through the original kernel to construct a candidate successor world.
4. The candidate is validated.
5. One complete checksummed commit frame is appended and `fsync`ed.
6. The new immutable world is published.
7. Recovery rebuilds the database by replaying complete valid frames.

The append log is authoritative. Active heads, indexes, and query structures are derived materializations of the last durable valid world.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design contract and [STATUS.md](STATUS.md) for the current evidence, scope, and known limitations.

## Repository Map

| File | Role | Status |
| --- | --- | --- |
| `forthdb_kernel.py` | Canonical semantic kernel | Foundation |
| `library_demo.py` | Original regression suite and library application | Baseline evidence |
| `forthdb_atomic.py` | First in-memory atomic publication experiment | Superseded, retained for research history |
| `atomic_demo.py` | Tests for the first atomic experiment | Historical evidence |
| `forthdb_world.py` | Durable committed-world transaction and recovery model | Current implementation direction |
| `world_library_demo.py` | Library integration, ACID, recovery, and corruption tests | Primary current evidence |
| `ARCHITECTURE.md` | Committed-world design contract | Current documentation |
| `STATUS.md` | Research status and reproducibility guide | Current documentation |

## Reproduce the Evidence

The repository currently uses only the Python standard library.

```bash
python library_demo.py
python atomic_demo.py
python world_library_demo.py
```

`library_demo.py` verifies the original semantic kernel independently of persistence or transactions.

`atomic_demo.py` preserves the intermediate experiment that demonstrated private staging, all-or-nothing publication, and rollback before the committed-world design was discovered.

`world_library_demo.py` runs the library application through the durable committed-world model and tests snapshot behavior, stale-writer rejection, constraint failure, `fsync` ordering, crash recovery, incomplete-tail handling, and corruption detection.

## Current ACID Contract

The committed-world model is an executable ACID model within its stated concurrency boundary.

### Atomicity

A transaction is represented by one complete commit frame. An incomplete trailing frame is not a world and is ignored during recovery. No partially applied transaction is visible.

### Consistency

A candidate successor world must satisfy the kernel invariants and any registered application constraints before its frame is appended. Invalid candidates never become committed worlds.

### Isolation

Readers capture one immutable world and remain on it for the duration of their work. A transaction may commit only if the world it was based on is still current; stale writers abort rather than silently overwrite a later commit.

### Durability

A complete checksummed frame is appended and `fsync`ed before the new in-memory world is published or commit success is returned. Recovery treats the last complete valid frame as the current database.

The present implementation serializes writers inside one process. Cross-process writer exclusion and more permissive conflict detection are implementation problems reserved for later work; they do not change the committed-world contract.

## The Spirit of the Project

There are many excellent databases. This project is not an attempt to dismiss or casually replace them.

ForthDB is driven by curiosity, experimentation, and evidence rather than feature lists or benchmark claims. Whenever possible, we begin with the smallest executable idea capable of answering a clear question. We build it, observe it, preserve successful behavior as regression tests, and only then decide whether the explanation should change.

The goal is not to invent complexity.

The goal is to discover simplicity.

Ideas are hypotheses.

Code is an experiment.

Regression tests are evidence.

The current architecture is our best explanation of the evidence gathered so far.

## Our Method

The method is as important as the design.

Our typical cycle is:

1. Ask a clear question.
2. Build the smallest executable model capable of answering it.
3. Run an existing application through the model whenever possible.
4. Observe the results and failure modes.
5. Preserve successful behavior as regression tests.
6. Keep the original semantic implementation available as a reference.
7. Let repeated evidence—not intuition or anticipated performance—justify changes.

New models should generally interpret their operations through `forthdb_kernel.py` rather than independently reimplementing ForthDB semantics. This keeps storage and transaction experiments comparable and prevents the project from losing the behavior already earned by earlier work.

## Working Beliefs

We currently believe:

- Search and organization are more fundamental than computation.
- Stable identities should outlive human-readable names.
- History should be preserved rather than overwritten by default.
- Append-oriented writing can make atomic publication and recovery simpler.
- A transaction can be understood as the creation of a valid successor world.
- Authoritative immutable state should be separated from derived indexes and caches.
- Small primitives are preferable to large frameworks.
- Applications and reproducible failures should reveal missing primitives.

These are working beliefs, not immutable truths. The project exists to test them.

## Inspiration

This project was initially inspired by ideas from the Forth programming language.

Forth demonstrated that rich systems can emerge from a remarkably small set of orthogonal primitives. That philosophy influenced this work, particularly its preference for simple kernels, composability, and compiled execution.

The goal, however, is not to build a Forth database. Ideas are retained because experiments support them, not because they resemble Forth.

## Success

Success is not measured by replacing existing databases or winning premature benchmarks.

Success is measured by whether this exploration teaches us something true about how databases, query engines, and information systems can be built from small, composable, immutable primitives.

The purpose of this repository is not to defend an idea.

It is to investigate one honestly.
