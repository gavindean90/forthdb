# ForthDB Rust

This workspace contains the Rust semantic kernel and committed-world engine.

## Implemented milestones

1. `MemoryCommitStore`
2. `FileCommitStore`
3. `MmapCommitStore`
4. `IoUringCommitStore`
5. Structurally shared worlds

All stores implement the same `CommitStore` contract. Transactions derive and validate a private candidate, append its canonical `CommitFrame`, wait for required durability, and only then publish the immutable successor.

Milestone 5 replaced full semantic-kernel cloning with structurally shared record chunks, persistent maps and sets, incremental validation, and background root retirement. It did not modify commit frames, world identity, recovery, or any commit-store implementation. The old kernel remains available as `LegacyForthDb` for differential tests.

Full Milestone 5 design and evidence: [`STRUCTURAL_SHARING.md`](STRUCTURAL_SHARING.md).

Format contract: [`FILE_FORMAT.md`](../FILE_FORMAT.md).

World contract: [`WORLD_CONTRACT.md`](../WORLD_CONTRACT.md).

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

The 1,000-snapshot retirement test observed foreground P99 near 1.15 µs. Recursive destruction was transferred to the background reaper and measured separately; it was not eliminated.

## Correctness gates

Differential tests compare the shared and legacy kernels for define, redefine, forget, history, indexed queries, provenance, immutable clone isolation, and a deterministic randomized 10,000-operation sequence with periodic full audits.

The existing world, file, mmap, io_uring, conformance, recovery, and canonical-byte suites remain mandatory. Milestone 5 contains no commit-store implementation changes.

## Current boundaries

The engine does not yet implement queued durability, batching, group commit, checkpoints, compaction, crash fault injection, or cross-process writer coordination. Final database shutdown still releases the world-history spine synchronously.

The next experiment is queued durability: derive a private chain of shared successor worlds, persist one durability epoch, publish its tail once, and resolve each writer with its own immutable intermediate world. That visibility and durability contract must be specified before implementation.
