# ForthDB Rust

This workspace contains the Rust semantic kernel and committed-world engine.

## Implemented milestones

1. `MemoryCommitStore`
2. `FileCommitStore`
3. `MmapCommitStore`
4. Structurally shared worlds
5. Queued-intent semantic epoch control
6. Bounded ingress and commit tickets
7. Ordinary-file durability epochs
8. Opt-in speculative io_uring durability overlap

All stores implement the same `CommitStore` contract. Strict transactions derive and validate a private candidate, append its canonical `CommitFrame`, wait for required durability, and only then publish the immutable successor.

Milestone 5 replaced full semantic-kernel cloning with structurally shared record chunks, persistent maps and sets, incremental validation, and background root retirement. It did not modify commit frames, world identity, recovery, or any commit-store implementation. The old kernel remains available as `LegacyForthDb` for differential tests.

Milestone 6A added a distinct `QueuedIntent` model and a pure epoch planner. Queued intents delegate predecessor assignment, use intent-scoped temporary entities, evaluate preconditions against their assigned private predecessor, and may be rejected independently without consuming world or allocator state. The in-memory control appends accepted canonical frames and advances the global reader head once to the epoch tail. Strict transaction stale-writer semantics remain unchanged.

Milestone 6A.2 adds a fixed-capacity MPSC ingress and one in-memory committer thread. Admission is nonblocking and returns the original intent when the queue is full. Tickets expose queued, claimed, and resolved phases. Dropping a ticket never cancels its admitted intent; failed result delivery is observed separately after the authoritative history transition completes.

Milestone 6B adds `FileEpochStore` and `DurableQueuedIntentController`. The paired ordinary-file transport can either synchronize every frame or encode a contiguous frame arena and synchronize once per epoch. An accepted durable ticket resolves only after the file synchronization succeeds and the reader head advances to the epoch tail. Observed failures enter verified repair; any uncertain repair permanently poisons the live handle.

Milestone 6C tested three synchronous Linux io_uring epoch transports. None outperformed the ordinary-file epoch control, so those transports were retired. The remaining opt-in io_uring controller uses one contiguous `WRITE` plus `FSYNC(DATASYNC)` to overlap durability of epoch N with private preparation of epoch N+1. The ordinary-file per-epoch transport remains the default.

Design and evidence:

- [`STRUCTURAL_SHARING.md`](STRUCTURAL_SHARING.md)
- [`QUEUED_DURABILITY.md`](QUEUED_DURABILITY.md)
- [`FILE_EPOCHS.md`](FILE_EPOCHS.md)
- [`IO_URING_EPOCHS.md`](IO_URING_EPOCHS.md)
- [`FILE_FORMAT.md`](../FILE_FORMAT.md)
- [`WORLD_CONTRACT.md`](../WORLD_CONTRACT.md)

## Run locally

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
FORTHDB_M5_MILLION=1 \
cargo run --quiet --release --manifest-path rust/Cargo.toml \
  -p forthdb-bench --bin structural_isolated

FORTHDB_M6_RETAINED_DEFINITIONS=100000 \
cargo run --quiet --release --manifest-path rust/Cargo.toml \
  -p forthdb-bench --bin queued_epoch

cargo run --quiet --release --manifest-path rust/Cargo.toml \
  -p forthdb-bench --bin queued_ingress

cargo run --quiet --release --manifest-path rust/Cargo.toml \
  -p forthdb-bench --bin queued_file_epoch

cargo run --quiet --release --manifest-path rust/Cargo.toml \
  -p forthdb-bench --bin queued_io_uring_epoch

cargo run --quiet --release --manifest-path rust/Cargo.toml \
  -p forthdb-library -- /tmp/forthdb-library.fdb
```

The speculative io_uring controller is Linux-only and reports unavailability when ring creation is denied by the running kernel or security policy.

## Milestone 5 result

The accepted isolated run held the transaction delta at one definition:

| Retained definitions | Shared candidate | Legacy control | Shared allocation |
| ---: | ---: | ---: | ---: |
| 100 | 8.87 µs | 53.53 µs | 41,411 bytes |
| 1,000 | 14.22 µs | 575.18 µs | 59,442 bytes |
| 10,000 | 20.89 µs | 8.64 ms | 79,701 bytes |
| 100,000 | 28.55 µs | — | 86,312 bytes |
| 1,000,000 | 45.45 µs | — | 98,150 bytes |

A 10,000-fold retained-world increase produced about 5.1 times candidate latency rather than the former linear clone. Current-head reads remained in the tens-of-nanoseconds tier.

## Milestone 6A result

The accepted semantic-control run derived private epochs over a 100,000-definition base:

| Accepted intents | Median epoch | Median per intent |
| ---: | ---: | ---: |
| 1 | 38.71 µs | 38.71 µs |
| 4 | 121.22 µs | 30.30 µs |
| 16 | 438.90 µs | 27.43 µs |
| 64 | 1.63 ms | 25.54 µs |

Epoch time scaled approximately with accepted-intent count, while per-intent derivation remained roughly 25–39 µs. This is the baseline for later durability amortization, not a file or io_uring throughput claim.

## Milestone 6A.2 result

The accepted integrated observation used a 100,000-definition base, ingress capacity 256, maximum batch 16, and cooperative retry after immediate backpressure:

| Workload | Accepted intents | Average batch | Intents/s |
| --- | ---: | ---: | ---: |
| One producer | 1,024 | 15.75 | 6,329 |
| Four producers | 4,096 | 15.94 | 7,318 |
| Eight producers | 8,192 | 16.00 | 11,080 |
| Four producers with abandoned tickets | 4,096 | 16.00 | 7,309 |

The abandoned workload committed every intent while reporting 4,096 abandoned tickets and 4,096 failed completion deliveries. Concurrent stress tests also prove one unique committed version per successful admission, no loss or duplication, bounded queue depth, and abandonment safety both before and after claim.

## Milestone 6B result

The accepted paired ordinary-file benchmark used three alternating rounds over a 100,000-definition base. Batch size one is the no-amortization control.

| Policy | Max batch | Median intents/s | Median syncs | Syncs/intent |
| --- | ---: | ---: | ---: | ---: |
| Per-frame | 1 | 2,502 | 512 | 1.0000 |
| Per-epoch | 1 | 2,494 | 512 | 1.0000 |
| Per-frame | 16 | 2,620 | 2,048 | 1.0000 |
| Per-epoch | 16 | 6,541 | 128 | 0.0625 |

At batch size one the policies converged, confirming that the paired harness does not manufacture an arena advantage. At batch size sixteen, one-sync epochs reduced synchronization calls by 16 times and increased median complete-pipeline throughput by approximately 2.50 times. Every run reopened with the exact expected canonical frame count.

Failure tests use real files and inject deterministic write, synchronization, truncation, and verification failures. They include every byte boundary of a three-frame arena and subprocess termination without Rust cleanup. Successful repair restores the exact pre-epoch bytes; any uncertain double fault poisons the live handle.

## Milestone 6C result

The accepted four-round rotating benchmark used the same complete durable controller over a 100,000-definition base. Batch size one remained the no-amortization control.

| Transport | Max batch | Median intents/s | Writes | Syncs | CQEs | Max writes in flight |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Ordinary per-epoch | 1 | 1,779 | 512 | 512 | 0 | 0 |
| io_uring contiguous `WRITE` | 1 | 1,631 | 512 | 512 | 1,024 | 1 |
| io_uring `WRITEV` | 1 | 1,762 | 512 | 512 | 1,024 | 1 |
| io_uring positional writes | 1 | 1,670 | 512 | 512 | 1,024 | 1 |
| Ordinary per-epoch | 16 | **8,183** | 128 | 128 | 0 | 0 |
| io_uring contiguous `WRITE` | 16 | 8,010 | 129 | 129 | 258 | 1 |
| io_uring `WRITEV` | 16 | 7,978 | 129 | 129 | 258 | 1 |
| io_uring positional writes | 16 | 7,425 | 2,048 | 129 | 2,177 | 16 |

`WRITEV` removed the contiguous arena copy but did not improve throughput. The pipelined form achieved genuine QD=16, yet its SQE and CQE overhead made it about 9 percent slower than one ordinary contiguous epoch write. The ordinary-file per-epoch transport therefore remains the default. These synchronous ring implementations have been removed; their results remain in [`IO_URING_EPOCHS.md`](IO_URING_EPOCHS.md) as a historical falsification record.

## Speculative io_uring result

The one-epoch-ahead experiment changed the concurrency proposition rather than
the write syscall. On GitHub Actions run `30838953811`, it prepared every
possible successor with no rederivation and reached median throughput of 2,566
versus 2,007 intents/s at batch size one, and 9,288 versus 6,915 at batch size
sixteen. The gain is consistent with overlapping semantic preparation with
durability. Ordinary `write` plus `fdatasync` remains the simpler default and
control; speculative io_uring remains opt-in.

## Correctness gates

The shared and legacy semantic kernels remain under differential testing. Milestone 6 adds sequential-versus-queued world and frame parity, byte-for-byte file parity, temporary-entity scope enforcement, predecessor-relative preconditions, independent rejection, one-tail publication, a deterministic 10,000-intent differential sequence, multi-producer admission/ticket stress, ordinary-file repair/poisoning tests, exhaustive byte-boundary failure injection, subprocess crash-prefix recovery, io_uring byte parity, explicit CQE correlation, malformed-completion rejection, and true-QD transport metrics.

The existing world, file, mmap, conformance, recovery, canonical-byte, and speculative io_uring suites remain mandatory. The epoch transports do not change `CommitFrame`, version 1 encoding, `FileCommitStore`, or `MmapCommitStore`.

## Current boundaries

The engine does not yet implement dwell-time batching, adaptive batch policy, multiple durability epochs in flight, a dedicated completion reactor, registered io_uring resources, checkpoints, compaction, true power-loss fault injection, worker restart, or cross-process writer coordination.

The opt-in [one-epoch-ahead io_uring experiment](SPECULATIVE_IO_URING.md) overlaps private preparation with durability while leaving ordinary per-epoch durability as the default and control.
