# ForthDB Rust

This workspace contains an independent Rust implementation of the ForthDB semantic kernel and the first storage-independent committed-world engine.

## Current scope

`forthdb-core` implements:

- typed entities, slots, literals, predicates, symbols, variables, facts, and patterns
- immutable definition records
- one active definition head per slot
- `define`, `resolve`, `definitions`, `forget`, and complete operation history
- active-head indexes for exact and partial fact lookup
- variable matching and multi-pattern joins
- distinct query results and provenance
- display names, symbol binding, and compiled stable identity
- deep cloning for private candidate-world construction

`forthdb-conformance` parses `conformance/v1/kernel_cases.json`, executes every step through the Rust kernel, and compares every checked result with the language-neutral contract.

The current conformance result is:

```text
4 cases
33 operations
13 checked assertions
status: passed
```

`forthdb-world` implements Milestone 1 of `WORLD_CONTRACT.md`:

- `WorldId`
- immutable `World` snapshots
- ordered transaction operations
- private `CandidateWorld` construction
- kernel and application validation
- deterministic logical world identity
- stale-writer rejection
- append-before-publication ordering
- atomic replacement of the current `Arc<World>`
- the `CommitStore` abstraction
- `MemoryCommitStore`
- in-memory frame enumeration and logical reconstruction

The current candidate implementation deep-clones the base semantic kernel and applies only the staged transaction operations to that private clone. Existing readers retain their original immutable `Arc<World>`.

`forthdb-bench` contains separate release-mode observational benchmark binaries for the semantic kernel and committed-world engine.

## Run locally

From the repository root:

```bash
cargo test --manifest-path rust/Cargo.toml

cargo run --quiet --manifest-path rust/Cargo.toml \
  -p forthdb-conformance -- conformance/v1/kernel_cases.json

cargo run --quiet --release --manifest-path rust/Cargo.toml \
  -p forthdb-bench --bin forthdb-bench

cargo run --quiet --release --manifest-path rust/Cargo.toml \
  -p forthdb-bench --bin world
```

The benchmark commands print JSON containing workload dimensions, three elapsed samples, median/minimum/maximum nanoseconds per operation, operations per second, checksums, build profile, platform, and available GitHub metadata.

## Semantic-kernel baseline

The first complete kernel benchmark was GitHub Actions run `30770445865`, using a release build on Linux x86-64. These values are observations from a shared hosted runner, not stable performance guarantees.

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

Retaining 50,000 prior definitions did not measurably increase current-head resolution in that run. All three medians remained approximately 24.7 ns, consistent with current resolution following the active head rather than scanning history.

## MemoryCommitStore baseline

The accepted Milestone 1 benchmark was GitHub Actions run `30772620993`, using a release build on Linux x86-64.

| Workload | Median | Approx. throughput | Shape |
| --- | ---: | ---: | --- |
| Candidate from genesis, 1 operation | 1.97 µs | 506,796 candidates/s | Clone empty base, apply and validate one definition |
| Candidate from genesis, 10 operations | 23.03 µs | 43,424 candidates/s | Ten definitions |
| Candidate from genesis, 100 operations | 220.44 µs | 4,536 candidates/s | One hundred definitions |
| Candidate from genesis, 1,000 operations | 2.94 ms | 341 candidates/s | One thousand definitions |
| One-operation candidate on 100-definition world | 86.49 µs | 11,563 candidates/s | Deep-clone base, apply one definition |
| One-operation candidate on 1,000-definition world | 1.52 ms | 659 candidates/s | Deep-clone base, apply one definition |
| One-operation candidate on 10,000-definition world | 18.33 ms | 54.6 candidates/s | Deep-clone base, apply one definition |
| Capture immutable snapshot | 11.20 ns | 89.3 million snapshots/s | Read lock plus `Arc` clone |
| Sequence of 1,000 one-operation commits | 483.93 µs/commit | 2,066 commits/s | Clone, validate, append, and publish while history grows |
| Reconstruct 1,000 no-op frames | 138.53 µs | 7,219 reconstructions/s | Logical in-memory replay and identity verification |

An earlier correctness-first version reconstructed every candidate by replaying its complete operation history. It measured approximately 10.1 ms per commit over the same growing 1,000-commit sequence. Deep-cloning the immutable base world reduced that observation to approximately 0.484 ms per commit, about a 20-fold improvement.

The remaining candidate cost is explicit: the current deep clone copies retained definitions and indexes, so candidate construction still grows with world size. This is not caused by `CommitStore` or publication. Structural sharing is the clear future optimization for the in-memory world representation, while the storage roadmap can proceed independently through the `CommitStore` boundary.

## Benchmark boundaries

The current world measurements include:

- private candidate construction
- kernel validation
- in-memory commit-frame append
- immutable snapshot capture
- stale-world publication semantics
- logical reconstruction from memory frames

They do not include:

- serialization
- checksums
- filesystem writes or `fsync`
- mmap
- io_uring
- checkpoints
- process-crash recovery
- cross-process writer coordination
- application-scale Rust library or deployment workloads

Hosted runners are noisy and may differ in CPU model, placement, contention, and virtualization. Benchmark numbers therefore do not fail ordinary commits. Semantic conformance and committed-world correctness remain required gates; timing remains reported evidence.

The figures are not comparisons with Python, SQLite, RocksDB, PostgreSQL, or another database. Such comparisons require deliberately matched workloads and contracts.

## Next storage milestone

The next storage milestone is `FileCommitStore`: canonical commit-frame encoding, append, synchronization, reopening, and fail-closed validation of established history using ordinary file I/O.

That milestone must preserve the same `World`, `Transaction`, `CandidateWorld`, `CommitFrame`, and `CommitStore` semantics. Mmap and io_uring remain later storage implementations, not changes to the transaction model.
