# forthdb

> **Current milestone:** Preserve the original Python kernel as the semantic reference implementation. Test the committed-world architecture by running substantially different applications through both the bare kernel and the durable model, then require their shared behavior to remain identical.

ForthDB is an experimental database project built from a small set of composable ideas:

- stable entity identities
- immutable definitions
- named slots with one active definition
- reversible definition history
- indexed fact queries and joins
- human-readable symbols compiled to stable identities
- transactions that create immutable successor worlds

The project began with a simple question:

> *If we forgot what a database is supposed to look like and started from a very small set of ideas, what would naturally emerge?*

The current answer is an append-oriented key-value and fact database in which a transaction constructs, validates, durably records, and publishes a complete new world rather than mutating the existing one.

## Where the Project Is Now

The original kernel remains the canonical definition of ForthDB semantics. It establishes what entities, facts, slots, definitions, `define`, `forget`, symbol compilation, indexes, and queries mean.

The durable model wraps those semantics in a committed-world architecture:

1. A transaction captures one committed world.
2. Its operations are staged privately.
3. The operations are applied through the original kernel to construct a candidate successor world.
4. The candidate is validated.
5. One complete checksummed commit frame is appended and `fsync`ed.
6. The new immutable world is published.
7. Recovery rebuilds the database by replaying complete valid frames.

The append log is authoritative. Active heads, indexes, and query structures are derived materializations of the last durable valid world.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design contract and [STATUS.md](STATUS.md) for the current evidence, scope, and known limitations.

## Application Evidence

ForthDB now has two application pairs. Each application runs once directly against the semantic kernel and once through the committed-world database.

### Library application

The library model stresses:

- graph traversal
- changing relationships
- stable compiled identity
- display-name changes and symbol rebinding
- duplicate facts and provenance
- movement, checkout, return, and definition history

Files:

- `library_demo.py`
- `world_library_demo.py`

### Deployment control-plane application

The deployment model stresses a different shape of workload:

- coordinated multi-slot release publication
- application constraints over a dependency graph
- desired state versus observed state
- progressive convergence
- immutable snapshots of the pre-release world
- stale deployment operators
- rollback represented as a new committed decision
- recovery of a partially converged operational world

It models three services—API, Worker, and Schema—whose versions have dependencies. A release is accepted only when it has approval, targets Production, selects exactly one version per service, includes all required versions, matches the proposed desired state, and becomes Production’s current release.

A deliberately incompatible release is rejected without creating a world or appending durable bytes. A compatible release changes all desired versions atomically. Observed versions then converge through later commits. A rollback creates another world restoring the earlier desired versions while preserving the failed rollout in history.

Files:

- `deployment_demo.py`
- `world_deployment_demo.py`

## Repository Map

| File | Role | Status |
| --- | --- | --- |
| `forthdb_kernel.py` | Canonical semantic kernel | Foundation |
| `library_demo.py` | Original library application and kernel regression | Baseline evidence |
| `deployment_demo.py` | Deployment control-plane application on the bare kernel | Second semantic application |
| `forthdb_atomic.py` | First in-memory atomic publication experiment | Superseded, retained for research history |
| `atomic_demo.py` | Tests for the first atomic experiment | Historical evidence |
| `forthdb_world.py` | Durable committed-world transaction and recovery model | Current implementation direction |
| `world_library_demo.py` | Library application through committed worlds | Durable application evidence |
| `world_deployment_demo.py` | Deployment application through committed worlds | Second durable application |
| `research_regression.py` | Unified suites, cross-model comparison, recovery, and determinism checks | Primary research regression |
| `.github/workflows/research-regression.yml` | Clean-machine execution and evidence publication | Automated witness |
| `ARCHITECTURE.md` | Committed-world design contract | Current documentation |
| `STATUS.md` | Research status and reproducibility guide | Current documentation |

## Reproduce the Evidence

The repository currently uses only the Python standard library.

The primary command is:

```bash
python research_regression.py
```

It runs all component suites and verifies that:

- both applications execute against the original semantic kernel
- both applications execute through the committed-world model
- the kernel and durable versions produce identical shared semantic projections
- restart recovery reconstructs each live committed world
- independent Python processes with different hash seeds produce the same world digests
- those processes produce byte-identical durable logs
- generated application projections are deterministic

It writes machine-readable and human-readable reports to `artifacts/` by default.

The individual stages remain directly runnable:

```bash
python library_demo.py
python deployment_demo.py
python atomic_demo.py
python world_library_demo.py
python world_deployment_demo.py
```

## GitHub Actions

The **Research Regression** workflow runs on pushes to `main`, pull requests, and manual dispatches.

GitHub Actions is not a second definition of correctness. The assertions live in `research_regression.py` so they can also be executed locally. Actions contributes a clean, independent machine that knows only what is committed to the repository.

A green workflow means that a fresh checkout can:

- compile every Python artifact
- pass the semantic, historical atomic, committed-world, library, and deployment suites
- reproduce both applications through the original and durable models
- verify cross-model semantic continuity
- verify crash recovery and deterministic durable history
- complete without modifying the checked-out repository

Each run places the Markdown report in the job summary and uploads the JSON and Markdown reports as a workflow artifact. Observational values such as log size, record count, and world version are reported; they are not automatically treated as permanent architectural contracts.

## Current ACID Contract

The committed-world model is an executable ACID model within its stated concurrency boundary.

### Atomicity

A transaction is represented by one complete commit frame. An incomplete trailing frame is not a world and is ignored during recovery. No partially applied transaction is visible.

The deployment application demonstrates this at application scale: Production never exposes a desired state containing only part of a release.

### Consistency

A candidate successor world must satisfy the kernel invariants and any registered application constraints before its frame is appended. Invalid candidates never become committed worlds.

The deployment application demonstrates graph-wide compatibility validation before publication.

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

Applications are challenges to the model.

Regression tests are evidence.

The current architecture is our best explanation of the evidence gathered so far.

## Our Method

Our typical cycle is:

1. Ask a clear question.
2. Build the smallest executable model capable of answering it.
3. Run an existing application through the model whenever possible.
4. Add a substantially different application before assuming generality.
5. Observe results and failure modes.
6. Preserve successful behavior as regression tests.
7. Keep the original semantic implementation available as a reference.
8. Let repeated evidence—not intuition or anticipated performance—justify changes.

New models should generally interpret their operations through `forthdb_kernel.py` rather than independently reimplementing ForthDB semantics. This keeps storage and transaction experiments comparable and prevents the project from losing the behavior already earned by earlier work.

## Working Beliefs

We currently believe:

- Search and organization are more fundamental than computation.
- Stable identities should outlive human-readable names.
- History should be preserved rather than overwritten by default.
- Append-oriented writing can make atomic publication and recovery simpler.
- A transaction can be understood as the creation of a valid successor world.
- Authoritative immutable state should be separated from derived indexes and caches.
- Desired state and observed state can coexist without pretending external processes are atomic.
- Rollback is usually a new decision, not erasure of the failed decision.
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
