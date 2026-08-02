# ForthDB Rust

This workspace contains an independent Rust implementation of the ForthDB semantic kernel.

## Current scope

`forthdb-core` now implements:

- typed entities, slots, literals, predicates, symbols, variables, facts, and patterns
- immutable definition records
- one active definition head per slot
- `define`, `resolve`, `definitions`, `forget`, and complete operation history
- active-head indexes for exact and partial fact lookup
- variable matching and multi-pattern joins
- distinct query results and provenance
- display names, symbol binding, and compiled stable identity

`forthdb-conformance` parses `conformance/v1/kernel_cases.json`, executes every step through the Rust kernel, and compares every checked result with the language-neutral contract.

The current conformance result is:

```text
4 cases
33 operations
13 checked assertions
status: passed
```

`forthdb-bench` provides a small release-mode observational benchmark harness for the conforming in-memory kernel.

## Run locally

From the repository root:

```bash
cargo test --manifest-path rust/Cargo.toml

cargo run --quiet --manifest-path rust/Cargo.toml \
  -p forthdb-conformance -- conformance/v1/kernel_cases.json

cargo run --quiet --release --manifest-path rust/Cargo.toml \
  -p forthdb-bench
```

The benchmark command prints JSON containing workload dimensions, three elapsed samples, median/minimum/maximum nanoseconds per operation, operations per second, checksums, build profile, platform, and available GitHub metadata.

## First hosted-runner observation

The first complete benchmark run was GitHub Actions run `30770445865`, using a release build on Linux x86-64. These values are a baseline observation from a shared hosted runner, not stable performance guarantees.

| Workload | Median ns/op | Approx. ops/s | Workload shape |
| --- | ---: | ---: | --- |
| Define unique slots | 6,017.78 | 166,174 | 50,000 definitions, including all index updates |
| Redefine one slot | 2,040.03 | 490,190 | 50,000 retained definitions behind one active head |
| Resolve, history depth 1 | 24.73 | 40,443,075 | 500,000 resolutions |
| Resolve, history depth 1,000 | 24.68 | 40,511,820 | 500,000 resolutions |
| Resolve, history depth 50,000 | 24.76 | 40,392,698 | 500,000 resolutions |
| Exact fact query | 670.46 | 1,491,512 | One exact result in 20,000 active facts |
| Subject-predicate query | 707.02 | 1,414,389 | One bound object in 20,000 active facts |
| Two-hop join, fanout 64 | 94,697.66 | 10,560 | 64 joined rows per query |
| Forget to previous head | 1,919.10 | 521,077 | 10,000 forgets from a 30,000-definition chain |

The most important first result is the current-head resolution experiment. Retaining 50,000 prior definitions did not produce a measurable increase over a one-definition slot in this run: all three medians were approximately 24.7 ns. That is consistent with the intended architecture, where current resolution follows the active head rather than scanning history.

The complete JSON report remains attached to each GitHub Actions run as `rust-kernel-benchmarks.json`.

## Benchmark boundaries

These measurements currently cover only the in-memory semantic kernel. They do not include:

- transaction candidate construction
- committed worlds
- serialization
- checksums
- filesystem writes or `fsync`
- restart recovery
- cross-process writer coordination
- application-scale library or deployment workloads

Hosted runners are noisy and may differ in CPU model, placement, contention, and virtualization. Benchmark numbers therefore do not fail ordinary commits. Semantic conformance remains a required gate; timing remains reported evidence.

The current figures are also not comparisons with Python, SQLite, RocksDB, PostgreSQL, or another database. Such comparisons would require deliberately matched workloads and contracts.

## Next Rust milestone

The next bounded step is an immutable committed-world model over the conforming Rust kernel. It should first reproduce candidate construction, snapshot reads, validation, stale-writer rejection, and deterministic logical publication in memory. Durable frame benchmarks should follow only after those transaction semantics pass a cross-language contract.
