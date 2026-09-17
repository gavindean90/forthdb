# AGENTS.md

This file defines how coding agents should operate in the ForthDB repository.
It is intentionally not a second architecture specification. Agents and human
contributors should derive technical meaning from the same project documents.

## Working model

ForthDB is developed as a research system with executable evidence. Prefer
small, testable changes that preserve documented contracts and accumulated
regression evidence.

Do not infer a new architectural contract from implementation alone. If code,
tests, and documentation disagree, surface the discrepancy and resolve it
explicitly rather than silently choosing one source.

Do not remove or "clean up" older implementations merely because they appear
superseded. Some are retained as historical controls, semantic references, or
falsification evidence.

## Read before changing

Use the narrowest authoritative documentation set that governs the change.

- Repository orientation and project history: `README.md`
- Architectural layering and preservation rules: `ARCHITECTURE.md`
- Current committed-world semantic contract: `WORLD_CONTRACT.md`
- Durable file-format behavior: `FILE_FORMAT.md`
- Stable Semantic ISA contract: `SEMANTIC_ISA_SPEC.md`
- AST-to-SISA lowering rules: `AST_TO_SISA_LOWERING.md`
- Current Rust engine, milestones, commands, and correctness gates: `rust/README.md`
- Historical Python-era research status: `STATUS.md`

For Rust subsystem work, also read the specific design document associated with
the subsystem being changed. Examples include:

- `rust/STRUCTURAL_SHARING.md`
- `rust/QUEUED_DURABILITY.md`
- `rust/FILE_EPOCHS.md`
- `rust/IO_URING_EPOCHS.md`
- `rust/ADMISSION_EPOCH_WORLDS.md`
- `rust/DURABLE_TOKEN_VM.md`
- `rust/VM_DIRECT_QUERIES.md`
- `rust/LIFECYCLE_SAFETY.md`

The subsystem document may describe implementation detail or experimental
evidence. Root-level contracts remain authoritative for semantics that they
explicitly define.

## Change discipline

When modifying ForthDB:

1. Identify the contract or research question affected by the change.
2. Read the governing documentation before editing implementation code.
3. Preserve existing semantic and durability invariants unless the task
   explicitly changes them.
4. Prefer the smallest coherent implementation that can answer the question.
5. Extend tests or conformance evidence for behavior changes.
6. Update shared project documentation when a contract, architecture, or
   established result changes.
7. Keep experimental findings, including negative results, when they explain
   why the current design exists.

Do not change documented semantics merely to make an implementation easier.
Do not promote an implementation observation into a repository-wide contract
without explicit documentation and evidence.

## Important invariants

Unless a task explicitly changes a documented contract, preserve these rules:

- Committed Worlds are immutable logical states.
- Transactions and queued intents construct private successors rather than
  mutating a published World.
- Durable history is authoritative; derived indexes and caches are rebuildable.
- Rollback is represented by a later committed World, not history deletion.
- Strict transactions retain stale-writer rejection semantics.
- A rejected candidate or intent must not consume committed history or allocator
  state unless the governing contract explicitly says otherwise.
- Durability and publication ordering must follow the active store/controller
  contract.
- Corruption must fail closed where the documented format requires it.
- Existing public format and Semantic ISA contracts must remain stable unless a
  versioned change is intentional.

## Historical and reference implementations

Treat historical code as evidence, not dead weight.

The Python semantic kernel and applications remain useful references for the
meaning and evolution of ForthDB. The Rust engine contains newer implementation
work and extensive differential, conformance, recovery, and durability evidence.
Do not rewrite one to resemble the other unless the change has a specific
semantic or experimental purpose.

When a newer implementation replaces an older mechanism, preserve tests or
controls that still establish semantic continuity or document a falsified
approach.

## Validation

Run the narrow tests relevant to the change first, then the broader correctness
gates appropriate to the affected layer.

Common commands include:

```bash
python research_regression.py
python conformance_runner.py
cargo test --manifest-path rust/Cargo.toml
cargo run --quiet --manifest-path rust/Cargo.toml \
  -p forthdb-conformance -- conformance/v1/kernel_cases.json
```

Do not run expensive release benchmarks unless the task changes performance,
concurrency, durability transport, allocation behavior, or a benchmarked claim.
When performance is the question, compare against an appropriate existing
control and preserve enough information to reproduce the result.

Linux-specific io_uring behavior should not be assumed available on every
machine or CI environment.

## Documentation rules

Keep durable knowledge in normal repository documentation so humans and agents
share the same source of truth.

Use `AGENTS.md` for repository operating rules, navigation, validation
expectations, and agent-specific workflow constraints. Do not duplicate detailed
architecture or subsystem specifications here.

When implementation and documentation diverge, do not silently patch around the
mismatch. Determine whether the implementation is wrong, the document is stale,
or the contract is intentionally changing, then make that resolution visible in
the change.

## Completion criteria

Before declaring work complete:

- the change satisfies the requested behavior or research question;
- relevant tests and conformance checks pass;
- preserved contracts still hold, or intentional contract changes are documented;
- comments and docs do not claim evidence the change did not establish;
- historical controls and negative experimental evidence remain intact unless
  their removal is itself justified;
- generated artifacts, caches, benchmark output, and local state are not
  accidentally committed.

If a task reveals an unresolved architectural contradiction, report it instead
of hiding it behind a local implementation workaround.
