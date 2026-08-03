# ForthDB Rust

This workspace contains an independent Rust implementation of the ForthDB semantic kernel and committed-world engine.

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

`forthdb-world` implements Milestones 1, 2, and 3 of `WORLD_CONTRACT.md`:

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
- `FileCommitStore`
- `MmapCommitStore`
- canonical versioned commit-frame encoding
- synchronized append before publication
- reopening and logical reconstruction
- incomplete-tail recovery
- fail-closed corruption handling
- mapped frame-offset indexing
- borrowed zero-copy record and payload slices
- remapping after successful durable appends

The candidate implementation deep-clones the base semantic kernel and applies only the staged transaction operations to that private clone. Existing readers retain their original immutable `Arc<World>`.

`FileCommitStore` writes the version 1 format specified in `../FILE_FORMAT.md` using ordinary file I/O and `sync_data()`. `MmapCommitStore` maps those exact bytes for validation, recovery, and direct borrowed access to persisted records and payloads. The existing file store remains the authoritative append implementation; the mapped store remaps after a successful synchronized append.

A post-durability remap failure is recorded as an optimization failure rather than returned as a failed commit. That avoids reporting failure after a frame has already been synchronized. Cross-process mutation or truncation while a map is live remains outside the current contract.

`forthdb-bench` contains separate release-mode observational benchmark binaries for the semantic kernel, committed-world engine, file commit store, and mmap commit store.

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

cargo run --quiet --release --manifest-path rust/Cargo.toml \
  -p forthdb-bench --bin file

cargo run --quiet --release --manifest-path rust/Cargo.toml \
  -p forthdb-bench --bin mmap
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

The remaining candidate cost is explicit: the current deep clone copies retained definitions and indexes, so candidate construction still grows with world size. This is not caused by `CommitStore` or publication. Structural sharing is the clear future optimization for the in-memory world representation.

## FileCommitStore baseline

The first complete Milestone 2 benchmark was GitHub Actions run `30773123911`, using a release build on Linux x86-64 and the hosted runner's temporary filesystem.

| Workload | Median | Approx. throughput | Shape |
| --- | ---: | ---: | --- |
| 100 durable no-op commits | 464.24 µs/commit | 2,154 commits/s | Encode, append, `sync_data()`, and publish a fixed-size frame |
| 100 durable one-definition commits | 507.51 µs/commit | 1,970 commits/s | Growing deep-cloned world plus synchronized append |
| Reopen and reconstruct 100 frames | 73.25 µs | 13,652 reopens/s | Read, checksum, decode, validate, and reconstruct |
| Reopen and reconstruct 1,000 frames | 404.70 µs | 2,471 reopens/s | Full validation of 1,000 persisted no-op frames |
| Recover incomplete tail after 100 frames | 376.76 µs | 2,654 recoveries/s | Detect seven-byte tail, truncate, and synchronize |

The close spacing between no-op and one-definition durable commits shows that this small-world benchmark is primarily synchronization-bound: adding one definition increased the median by roughly 43 µs. These values are not device-independent guarantees, but they establish that the unbatched ordinary-I/O implementation already sustains roughly 2,000 individually synchronized commits per second on this runner.

Reopening 1,000 frames remained below half a millisecond in this observation. That result covers physical frame checks plus logical identity and invariant verification; it is not a checkpointed startup measurement.

## MmapCommitStore baseline

The accepted Milestone 3 benchmark was GitHub Actions run `30774026801`, using a release build on Linux x86-64 and the hosted runner's temporary filesystem.

| Workload | Median | Approx. throughput | Shape |
| --- | ---: | ---: | --- |
| Mmap open and reconstruct 100 frames | 103.64 µs | 9,648 opens/s | Map, scan, checksum, decode, validate, and reconstruct |
| Mmap open and reconstruct 1,000 frames | 463.44 µs | 2,158 opens/s | Full validation and reconstruction from mapped bytes |
| Mmap open and reconstruct 10,000 frames | 3.96 ms | 253 opens/s | Full validation and reconstruction from mapped bytes |
| Paired ordinary-file open, 1,000 frames | 419.81 µs | 2,382 opens/s | Same generated format and hosted run using `read_to_end` |
| Zero-copy mapped record lookup, 10,000 frames | 2.69 ns | 372.2 million lookups/s | Hot offset-directory lookup returning a borrowed record slice |
| Full mapped-byte scan, 10,000 frames | 112.80 µs/scan | 8,865 scans/s | Consume every mapped byte without an owned input copy |
| 100 synchronized mmap no-op commits | 315.02 µs/commit | 3,174 commits/s | Canonical append, `sync_data()`, publication, and remap |
| Recover mapped incomplete tail after 100 frames | 381.03 µs | 2,624 recoveries/s | Map, detect tail, unmap, truncate, synchronize, and remap |

Mmap did not improve full 1,000-frame reopening in this observation: the paired ordinary-file path was about 10% faster. The current startup path still decodes owned `CommitFrame` values and reconstructs the complete semantic world, so those costs dominate after the input bytes are available. Mapping removes the separate `read_to_end` input buffer, but it does not yet make semantic reconstruction zero-copy.

The new capability is direct indexed access to persisted bytes. Opening builds a compact frame-span directory, after which `mapped_record()` and `mapped_payload()` return borrowed slices into the mapping without allocation or payload decoding. The 2.69 ns result is a hot-cache microbenchmark of that narrow primitive, not an end-to-end database query or storage-device result.

The synchronized mmap commit observation remained in the same sub-millisecond class as `FileCommitStore`. Hosted-runner variance and benchmark ordering prevent treating the measured difference as a durable speedup claim. The relevant result is that remapping did not introduce an order-of-magnitude penalty while preserving the same synchronized append contract.

## Benchmark boundaries

The current measurements cover:

- semantic-kernel operations and current-head reads
- private candidate construction and validation
- immutable snapshot capture
- in-memory frame append and logical reconstruction
- canonical frame encoding and checksums
- ordinary file append and `sync_data()`
- reopening from disk
- memory-mapped frame scanning and validation
- borrowed zero-copy access to persisted record and payload bytes
- incomplete-tail truncation and remapping
- fail-closed tests for established corruption

They do not include:

- io_uring
- batching or group commit
- checkpoints
- zero-copy semantic object reconstruction
- structural sharing in candidate worlds
- compaction
- process-crash fault injection
- cross-process writer coordination
- application-scale Rust library or deployment workloads

Hosted runners are noisy and may differ in CPU model, placement, contention, filesystem, storage device, and virtualization. Benchmark numbers therefore do not fail ordinary commits. Semantic conformance and committed-world correctness remain required gates; timing remains reported evidence.

The figures are not comparisons with Python, SQLite, RocksDB, PostgreSQL, or another database. Such comparisons require deliberately matched workloads and contracts.

## Next storage milestone

The next storage milestone is `IoUringCommitStore`: preserve the same canonical frames and publication contract while moving append and synchronization submission behind Linux io_uring.

The initial io_uring milestone should measure single-commit overhead honestly before introducing queue depth, batching, or group commit. Mmap remains the read and persisted-byte access path; io_uring is the write-submission path.
