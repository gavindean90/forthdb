# ForthDB Rust

This workspace contains the independent Rust implementation of the ForthDB semantic kernel and committed-world engine.

## Implemented milestones

The Rust engine now implements the first four milestones in `WORLD_CONTRACT.md`:

1. `MemoryCommitStore`
2. `FileCommitStore`
3. `MmapCommitStore`
4. `IoUringCommitStore`

All four stores implement the same `CommitStore` contract. The transaction layer constructs and validates a private `CandidateWorld`, appends its canonical `CommitFrame`, and publishes the immutable successor only after the store reports success.

```text
begin from immutable World
          ↓
construct CandidateWorld
          ↓
validate kernel + application rules
          ↓
append canonical CommitFrame
          ↓
wait for durable completion when required
          ↓
publish Arc<World>
```

The semantic kernel retains complete definition history while maintaining one active head per slot. Current-state resolution therefore follows the active head rather than scanning prior definitions.

## Crates

### `forthdb-core`

The semantic kernel provides:

- typed entities, slots, literals, predicates, symbols, variables, facts, and patterns
- immutable definition records and complete operation history
- one active definition head per slot
- `define`, `resolve`, `definitions`, and `forget`
- active-head indexes for exact and partial fact lookup
- variable matching, multi-pattern joins, distinct results, and provenance
- display names, symbol binding, and stable compiled identity
- invariant validation and deep cloning for private candidate construction

### `forthdb-conformance`

The Rust kernel executes the language-neutral cases in `conformance/v1/kernel_cases.json`.

```text
4 cases
33 operations
13 checked assertions
status: passed
```

### `forthdb-world`

The committed-world engine provides:

- `WorldId`, immutable `World`, `Transaction`, `CandidateWorld`, and `CommitFrame`
- deterministic world identity
- read-your-own-writes candidate construction
- kernel and application validation
- stale-writer rejection
- append-before-publication ordering
- atomic replacement of the current `Arc<World>`
- logical reconstruction from committed frames
- incomplete-tail recovery and fail-closed corruption handling

Its storage implementations are:

#### `MemoryCommitStore`

An in-memory append-only frame vector used as the storage-independent semantic baseline.

#### `FileCommitStore`

Writes the version 1 format in `../FILE_FORMAT.md` using ordinary file I/O. Each commit is canonically encoded, appended, followed by `sync_data()`, and only then published.

#### `MmapCommitStore`

Maps the exact version 1 bytes for validation, recovery, and direct borrowed access through `mapped_record()` and `mapped_payload()`. A compact frame-span directory gives indexed access to persisted records without copying the file into an owned input buffer.

The ordinary file implementation remains the authoritative append path. A post-durability remap failure is recorded as an optimization failure instead of falsely reporting that an already-synchronized commit failed.

#### `IoUringCommitStore`

A Linux-only queue-depth-one durability backend. It preserves the exact version 1 bytes and submits one linked pair per commit:

```text
IORING_OP_WRITE + IO_LINK
            ↓
IORING_OP_FSYNC with DATASYNC
            ↓
verify both CQEs
            ↓
publish World
```

The ring contains two entries because one commit requires two linked operations, but only one commit may be in flight. The store verifies exact write length and synchronization completion and attempts to truncate back to the pre-append offset on failure. Recovery and incomplete-tail handling reuse the established `FileCommitStore` path so both stores accept the same history.

This first milestone intentionally contains no batching, group commit, registered buffers, registered files, SQPOLL, or multiple transactions in flight.

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

cargo run --quiet --release --manifest-path rust/Cargo.toml \
  -p forthdb-bench --bin io_uring
```

`IoUringCommitStore` is available only on Linux. Its benchmark emits an explicit unavailable result when the running kernel or security policy does not permit ring creation.

## Accepted observations

These figures come from shared GitHub-hosted Linux runners. They are reproducible observations, not stable guarantees or cross-database comparisons.

### Current-state reads

The original semantic-kernel run retained 1, 1,000, and 50,000 definitions behind a slot while current-head resolution remained approximately 24.7 ns in all three cases. That is the central intended property: retained history does not require a current-state history scan.

### Immutable snapshots

Capturing the current `Arc<World>` measured approximately 11 ns in the accepted Milestone 1 run.

### Candidate construction

The present candidate implementation deep-clones the base semantic kernel. A one-operation candidate therefore grows with retained world size:

| Retained world | Median candidate time |
| ---: | ---: |
| 100 definitions | 86.49 µs |
| 1,000 definitions | 1.52 ms |
| 10,000 definitions | 18.33 ms |

Structural sharing is the clear in-memory optimization frontier. It is independent of the storage transport work.

### File and mmap storage

Accepted earlier observations included:

| Operation | Median |
| --- | ---: |
| Individually synchronized `FileCommitStore` no-op commit | 464.24 µs |
| Reopen and reconstruct 1,000 file frames | 404.70 µs |
| Mmap open and reconstruct 1,000 frames | 463.44 µs |
| Hot zero-copy mapped-record lookup among 10,000 frames | 2.69 ns |
| Full mapped-byte scan of 10,000 frames | 112.80 µs |

Mmap did not accelerate complete recovery because owned frame decoding and semantic reconstruction still dominate. Its demonstrated contribution is direct indexed access to persisted bytes without a separate owned file buffer.

### Queue-depth-one io_uring

The accepted Milestone 4 benchmark is GitHub Actions run `30778808277`. It compared ordinary synchronized I/O and io_uring in the same release-mode process and temporary filesystem.

| Workload | Ordinary file I/O | io_uring | Difference |
| --- | ---: | ---: | ---: |
| 100 no-op commits | 254.85 µs/commit | 235.38 µs/commit | io_uring 7.6% faster |
| 100 one-definition commits | 252.88 µs/commit | 281.94 µs/commit | io_uring 11.5% slower |
| 1,000 no-op commits | 200.45 µs/commit | 238.14 µs/commit | io_uring 18.8% slower |
| Open existing 1,000-frame history through io_uring store | — | 514.41 µs | — |

The longer fixed-frame control is the best queue-depth-one comparison in this run. It shows that moving the same individually synchronized commit behind io_uring did not improve throughput by itself; the extra submission and completion machinery cost roughly 19% while the store waited after every commit.

That is an expected and useful baseline. The architecture now has a correct io_uring transport without conflating it with batching. Any later throughput gain must come from actually using the queue: multiple prepared writes, deeper submission, or a separately specified group-commit policy.

## Benchmark boundaries

The current evidence covers:

- semantic operations and current-head reads
- candidate construction and validation
- immutable snapshot capture
- in-memory append and logical reconstruction
- canonical frame encoding and checksums
- ordinary synchronized file append
- mapped scanning and borrowed persisted-byte access
- queue-depth-one linked io_uring write and data synchronization
- cross-store byte compatibility and reopening
- incomplete-tail recovery and established-corruption rejection

It does not yet cover:

- structural sharing in candidate worlds
- multiple io_uring commits in flight
- batching or group commit
- registered files or buffers
- SQPOLL
- checkpoints
- zero-copy semantic reconstruction
- compaction
- process-crash fault injection
- cross-process writer coordination
- application-scale Rust library or deployment workloads

## Next experiments

Milestone 4 completes the original storage sequence. Further work should now be selected by workload rather than numbered mechanically.

The two clearest independent experiments are:

1. **Structural sharing:** make candidate cost track transaction delta rather than retained world size.
2. **Queued durability:** prepare and submit multiple independent commit records while preserving an explicitly defined publication and durability contract.

Group commit is not merely a transport optimization. Publishing several transactions after one synchronization changes the durability and visibility contract and must be specified before implementation.
