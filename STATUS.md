# ForthDB Research Status

**Status date:** August 2, 2026

## Current State

ForthDB now has:

1. a semantic kernel
2. an in-memory atomic publication experiment retained as research history
3. a durable committed-world transaction model
4. two application pairs that run through both the bare kernel and the durable model
5. a unified regression executed locally and by GitHub Actions

The current implementation direction remains the committed-world model in `forthdb_world.py`.

The original kernel remains the semantic reference and should not be discarded or bypassed. The committed-world model constructs and recovers each world by applying operations through that kernel.

The public repository is now self-verifying: a clean machine can run both applications, compare the original and durable semantic projections, test recovery, verify deterministic logs, and publish a structured report without relying on conversation context.

## Application Evidence

### Library application

Files:

- `library_demo.py`
- `world_library_demo.py`

The library model demonstrates:

- two-hop graph traversal
- redefinition and current-state lookup
- restoration through `forget`
- duplicate assertions and provenance
- deep history with one-record current lookup
- compiled identity surviving display-name changes and symbol rebinding
- movement, checkout, and return
- snapshot stability and durable recovery

### Deployment control-plane application

Files:

- `deployment_demo.py`
- `world_deployment_demo.py`

The deployment model deliberately tests a different workload. It represents:

- three services: API, Worker, and Schema
- old and proposed versions
- version dependencies
- Production deployment targets
- release approval
- desired state
- observed state
- progressive convergence
- rollback as a later committed decision

The application demonstrates:

- an incompatible release is rejected
- rejection appends no durable bytes and creates no world
- a compatible release changes all desired versions in one committed transition
- a snapshot captured before release continues to expose the complete earlier configuration
- external rollout progress may arrive through separate later commits
- drift can be queried while observed state converges toward desired state
- stale deployment operators abort instead of overwriting a later release
- rollback creates another valid world rather than erasing the failed rollout
- desired-version history remains available after rollback
- restart recovery reconstructs the rollback world and its remaining drift

The bare-kernel and committed-world applications produce the same shared semantic projection.

## File Status

### Foundation

#### `forthdb_kernel.py`

The canonical semantic reference implementation.

It defines entities, facts, slots, immutable definition records, active heads, history, indexes, query planning, joins, provenance, symbol resolution, compiled identity, and rendering.

#### `library_demo.py`

The original application and semantic baseline.

#### `deployment_demo.py`

The second application on the bare kernel. It establishes the semantic behavior of release validation, desired and observed state, convergence, rollback, and history independently of persistence.

### Historical experiment

#### `forthdb_atomic.py`

The first executable atomicity model. It introduced private staging, all-or-nothing in-memory publication, explicit rollback, and failure injection before publication.

It is superseded by `forthdb_world.py` but retained because it records an important step in the reasoning.

#### `atomic_demo.py`

Tests and demonstration for the historical atomic model.

### Current durable model

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

The library application through the committed-world model.

#### `world_deployment_demo.py`

The deployment control-plane application through committed worlds. It adds application-scale evidence for atomic publication, graph-wide constraints, desired-versus-observed state, stale operators, rollback, and recovery.

### Preservation infrastructure

#### `research_regression.py`

The primary unified research regression.

It currently runs five component suites:

- semantic kernel
- historical atomic model
- committed-world ACID model
- deployment semantic application
- deployment committed-world application

The current suite total is **21 tests**.

It also verifies, for both the library and deployment applications:

- execution against the semantic kernel
- execution through the committed-world model in two separate Python processes
- different `PYTHONHASHSEED` values
- cross-model semantic continuity
- restart recovery
- deterministic world identity
- byte-identical durable logs
- deterministic application projections

The generated report schema is version 2 and contains separate observations and semantic projections for both applications.

#### `.github/workflows/research-regression.yml`

The clean-machine witness for the research regression.

It runs on pushes to `main`, pull requests, and manual dispatches using Python 3.13. It compiles all source files, runs `research_regression.py`, places the Markdown report in the job summary, uploads both reports, and verifies that execution leaves the checkout unchanged.

The workflow is intentionally thin. Correctness remains defined by executable Python assertions that can also run locally.

## Reproduction Commands

Run the complete research regression:

```bash
python research_regression.py
```

Run the individual applications and experiments:

```bash
python library_demo.py
python deployment_demo.py
python atomic_demo.py
python world_library_demo.py
python world_deployment_demo.py
```

The project currently depends only on the Python standard library.

A change should not be considered an improvement if it breaks baseline kernel behavior, either application’s cross-model semantic continuity, or committed-world recovery evidence without an explicit and documented reason.

## Current Observations

These values describe the current implementation and are not automatically permanent contracts.

### Library committed-world run

- active slots: 26
- immutable records: 31
- world version: 6
- deterministic log size: 7,295 bytes
- world digest: `b2f1b004e1572d9eed38f431634bacf554c2ef741bf51161ac65656dd1e70072`

### Deployment committed-world run

- active slots: 74
- immutable records: 85
- world version: 7
- deterministic log size: 18,107 bytes
- world digest: `657224ccf1955bcd9e62a3d7d5d93e977a65651d226ed18ac0823f3cea1d8b36`

Both applications produce byte-identical logs across independent Python processes with different hash seeds.

## Current ACID Claim

The committed-world implementation is an executable ACID model within a single-process ownership boundary.

### Atomicity demonstrated

- staged operations are private
- a transaction is represented by one complete commit frame
- incomplete trailing frames are not applied
- successful publication exposes the complete successor world
- failed candidate construction or validation changes no committed world
- a deployment release cannot become visible as a partial desired-state update

### Consistency demonstrated

- candidate worlds run the kernel’s structural validation
- registered constraints may reject candidates before durability
- validators are checked for read-only behavior
- recovery recomputes and verifies the resulting world digest
- deployment compatibility is checked across the selected version graph before publication

### Isolation demonstrated

- readers remain pinned to immutable snapshots
- transactions read their own candidate state
- stale writers abort instead of overwriting a later world
- commits are serialized inside one process
- a pre-release reader continues to observe the entire old deployment configuration

### Durability demonstrated

- commit frames are appended and `fsync`ed before in-memory publication
- reopening the database recovers complete committed frames
- a simulated failure after `fsync` but before publication still recovers the transaction
- incomplete tails are ignored
- established-history corruption raises an error rather than being silently repaired
- deployment desired state, remaining drift, current release, version, and digest survive restart

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
- actual deployment execution or reconciliation agents

These remain implementation, optimization, and productization questions. They are not currently reasons to change the committed-world semantic contract.

## Multiple Writers

The current design decision remains:

> A transaction extends exactly the committed world on which it was based. If a later world has already been published, the stale transaction must not silently commit over it.

The deployment application now demonstrates this with two operators preparing different releases from the same parent world.

The current implementation enforces the rule among writers inside one process. Coordination among independent processes is deferred.

## Decisions to Preserve

1. `forthdb_kernel.py` remains the canonical semantic implementation.
2. Later models apply operations through the kernel instead of casually duplicating its behavior.
3. Applications are continuing compatibility tests, not disposable demos.
4. The append log of complete valid commit frames is authoritative.
5. A transaction creates an immutable successor world rather than mutating the current one.
6. Readers capture one immutable world.
7. A stale transaction aborts unless a future explicit conflict-resolution model says otherwise.
8. Indexes and active-head materializations are derived state.
9. Durability precedes in-memory publication and reported commit success.
10. Desired state and observed state are distinct claims.
11. Rollback is represented as a new committed decision.
12. Performance work follows evidence rather than anticipation.

## Current Research Questions

The second application reduced the risk that the architecture was merely library-shaped, but important questions remain:

- Which third application would challenge a substantially different axis?
- What should a stable long-term commit-frame format contain?
- Should definition identity eventually be independent of reference-kernel list position?
- What checkpoint form preserves deterministic world identity?
- What retention contracts should govern compaction and archival?
- Which multi-process commit coordinator best fits the committed-world model?
- When is coarse stale-world rejection too restrictive for real applications?
- Should application constraints become named, durable declarations or remain runtime code?
- How should external reconcilers associate observations with the committed desired world they attempted to realize?

## Milestone Summary

ForthDB now has more than one application-shaped proof.

It has:

- a stable semantic reference
- two substantially different application models
- an executable explanation of atomic publication
- a durable committed-world architecture
- a bounded ACID contract
- deterministic recovery evidence
- cross-model semantic continuity for both applications
- a successful GitHub Actions workflow that independently reproduces the evidence

The deployment model is the first strong evidence that committed worlds are useful beyond the original library domain. The next work should challenge or deepen that result without discarding the evidence that produced it.
