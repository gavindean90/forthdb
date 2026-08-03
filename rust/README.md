# ForthDB Rust

This workspace contains the Rust semantic kernel and committed-world engine.

## Implemented milestones

1. `MemoryCommitStore`
2. `FileCommitStore`
3. `MmapCommitStore`
4. `IoUringCommitStore`
5. Structurally shared worlds
6. Queued-intent semantic epoch control

All stores implement the same `CommitStore` contract. Strict transactions derive and validate a private candidate, append its canonical `CommitFrame`, wait for required durability, and only then publish the immutable successor.

Milestone 5 replaced full semantic-kernel cloning with structurally shared record chunks, persistent maps and sets, incremental validation, and background root retirement. It did not modify commit frames, world identity, recovery, or any commit-store implementation. The old kernel remains available as `LegacyForthDb` for differential tests.

Milestone 6A added a distinct `QueuedIntent` model and a pure epoch planner. Queued intents delegate predecessor assignment, use intent-scoped temporary entities, evaluate preconditions against their assigned private predecessor, and may be rejected independently without consuming world or allocator state. The in-memory control appends accepted canonical frames and advances the global reader head once to the epoch tail. Strict transaction stale-writer semantics remain unchanged.

Design and evidence:

- [`STRUCTURAL_SHARING.md`](STRUCTURAL_SHARING.md)
- [`QUEUED_DURABILITY.md`](QUEUED_DURABILITY.md)
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
cargo run --quiet --release --manifest-path rust/Cargo.toml \
  -p forthdb-bench --bin io_uring

FORTHDB_M5_MILLION=1 \
cargo run --quiet --release --manifest-path rust/Cargo.toml \
  -p forthdb-bench --bin structural_isolated

FORTHDB_M6_RETAINED_DEFINITIONS=100000 \
cargo run --quiet --release --manifest-path rust/Cargo.toml \
  -p forthdb-bench --bin queued_epoch
```

`IoUringCommitStore` is Linux-only and reports unavailability when ring creation is denied by the running kernel or security policy.

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

## Correctness gates

The shared and legacy semantic kernels remain under differential testing. Milestone 6A adds sequential-versus-queued world and frame parity, byte-for-byte file parity, temporary-entity scope enforcement, predecessor-relative preconditions, independent rejection, one-tail publication, and a deterministic 10,000-intent differential sequence.

The existing world, file, mmap, io_uring, conformance, recovery, and canonical-byte suites remain mandatory. Milestone 6A changes no commit-store implementation or version 1 encoding.

## Current boundaries

The engine does not yet implement a background ingress queue, tickets, dwell-time batching, ordinary-file durability epochs, group commit, deeper io_uring utilization, checkpoints, compaction, crash fault injection, or cross-process writer coordination.

The next stages are a bounded ingress/ticket controller followed by the ordinary-file epoch control with explicit repairing and poisoned states.
