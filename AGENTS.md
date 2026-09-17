# AGENTS.md

This file defines how coding agents should operate in the ForthDB repository.
It is intentionally not a second architecture specification. Agents and humans
derive technical meaning from the same project documents and executable evidence.

## ForthDB agent philosophy

ForthDB is developed through human-directed, agent-assisted research. Agents may
do substantial implementation work, but the repository—not the conversation—is
the project's durable memory.

### Humans and agents share one technical reality

There is no agent-only architecture.

Architecture, contracts, rationale, and research results belong in normal
repository documentation and executable evidence that a human contributor would
also use. `AGENTS.md` describes how an agent works with that material; it is not
a private substitute for it.

If an agent needs durable knowledge to work correctly, first ask whether that
knowledge belongs in ordinary project documentation, a test, or an executable
contract rather than in an agent-only instruction.

### Evidence outranks elegance

ForthDB evolves by constructing executable models and attempting to break its
current explanations. Failed experiments, superseded implementations, negative
benchmarks, and historical controls may be valuable evidence.

Do not optimize the history of the project into a cleaner story than actually
happened. A simpler repository is not necessarily a better repository if the
simplification destroys evidence explaining why the current architecture exists.

### Implementation answers questions

For substantial work, identify the research question or contract being changed
before writing code. Be able to say what observation would count against the
proposed approach.

Do not turn requests such as "make this faster" or "improve the architecture"
into an open-ended license to redesign the system. Prefer the smallest coherent
experiment capable of distinguishing between competing explanations.

A useful experiment can falsify its motivating idea.

### Conversation is working memory; the repository is durable memory

Do not rely on conversation history to preserve a decision, invariant, negative
result, or non-obvious constraint needed by future work. Put durable knowledge in
the appropriate shared document, test, conformance case, benchmark record, or
executable contract.

A future human or agent starting from a clean checkout should be able to
understand why the resulting system exists without reconstructing the
conversation that produced it.

### Do not make the architecture silently smarter

Implementation may reveal that the current model is incomplete or wrong. That is
useful evidence, not permission to quietly repair the architecture in code.

When code, tests, and documentation disagree, surface the discrepancy. Determine
whether the implementation is wrong, the document is stale, or the contract is
intentionally changing, then make that resolution visible.

Architectural evolution should be explicit, evidenced, and understandable to the
next contributor.

## Finding the governing contract

Do not read every Markdown file by default. Use the narrowest documentation set
that governs the work, then follow references when the change crosses a boundary.

The main entry points are:

- `README.md` — repository orientation and project history
- `ARCHITECTURE.md` — architectural layering and preservation rules
- `WORLD_CONTRACT.md` — committed-world semantic contract
- `FILE_FORMAT.md` — durable file-format behavior
- `SEMANTIC_ISA_SPEC.md` — stable Semantic ISA contract
- `AST_TO_SISA_LOWERING.md` — canonical AST-to-SISA lowering
- `rust/README.md` — current Rust engine, milestones, commands, and correctness gates
- `STATUS.md` — historical Python-era research status; evidence, not current roadmap

For Rust subsystem work, read the subsystem document associated with the code
being changed. The `rust/` design documents record both active contracts and
experimental evidence. Root-level contracts remain authoritative for semantics
they explicitly define.

## Change discipline

When modifying ForthDB:

1. State the contract, hypothesis, or research question affected by the change.
2. Read the governing shared documentation before editing implementation code.
3. Preserve existing semantic and durability invariants unless the task
   explicitly changes them.
4. Prefer the smallest coherent implementation or experiment that can answer the
   question.
5. Extend tests, conformance cases, or benchmark evidence when behavior changes.
6. Record durable conclusions in the repository rather than leaving them only in
   conversation.
7. Preserve negative results and historical controls when they explain why the
   current design exists.

Do not change documented semantics merely to make an implementation easier. Do
not promote an implementation observation into a repository-wide contract
without explicit documentation and evidence.

## Invariants worth knowing before you touch the engine

Unless a task explicitly changes a documented contract:

- Committed Worlds are immutable logical states.
- Transactions and queued intents construct private successors rather than
  mutating a published World.
- Durable history is authoritative; derived indexes and caches are rebuildable.
- Rollback is a later committed World, not deletion of history.
- Strict transactions retain stale-writer rejection semantics.
- A rejected candidate or intent does not consume committed history or allocator
  state unless the governing contract explicitly says otherwise.
- Durability and publication ordering follow the active store/controller
  contract.
- Corruption fails closed where the documented format requires it.
- Public format and Semantic ISA contracts remain stable unless a versioned
  change is intentional.

Treat historical code as evidence, not dead weight. The Python semantic kernel
and applications remain reference material for ForthDB's meaning and evolution.
The Rust engine contains newer implementation work plus differential,
conformance, recovery, and durability evidence. Do not rewrite one to resemble
the other merely for consistency of style.

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

Do not run expensive release benchmarks merely because they exist. Run them when
the task changes performance, concurrency, durability transport, allocation
behavior, or a benchmarked claim. Compare against an appropriate existing
control and preserve enough information to reproduce the result.

Linux-specific io_uring behavior should not be assumed available on every
machine or CI environment.

## Completion

Before declaring work complete, ask whether a future contributor with only a
clean checkout has everything needed to understand and verify the change.

Relevant tests and conformance checks should pass. Intentional contract changes
should be documented. Comments and docs should not claim evidence the work did
not establish. Historical controls and negative evidence should remain intact
unless their removal is itself justified. Generated artifacts, caches, benchmark
output, and local state should not be accidentally committed.

If the work reveals an unresolved architectural contradiction, report it instead
of hiding it behind a local implementation workaround.
