# Milestone 5: Structurally Shared Worlds

## Purpose

Milestone 5 tests whether ForthDB's committed-world abstraction permits a major in-memory representation change without modifying transaction meaning, canonical commit frames, recovery, or durability transports.

The experiment replaces full semantic-kernel cloning with a structurally shared topology while retaining the previous kernel as a differential oracle.

## Frozen boundaries

Milestone 5 does not change:

- `CommitFrame`
- version 1 canonical encoding
- `WorldId` calculation
- transaction operation ordering
- stale-writer rejection
- append-before-publication ordering
- `MemoryCommitStore`
- `FileCommitStore`
- `MmapCommitStore`
- file recovery and corruption rules

The final branch diff contains no source changes to any commit-store implementation.

## Shared topology

The semantic kernel now uses:

- Arc-backed append-only record chunks
- persistent linked per-slot history
- two-level, high-branching shard directories
- persistent hash maps within affected shards
- persistent hash sets for high-fanout query-index buckets
- incrementally maintained active-state signatures for candidate validation

A candidate clone shares untouched record chunks, shard groups, shard maps, and index buckets with its base. A mutation path-copies only the affected directories and persistent-map paths.

The public semantic behavior remains independent of internal node layout and iteration order.

## Validation

`ForthDb::validate()` checks incrementally maintained invariants suitable for candidate construction.

`ForthDb::validate_full()` performs an explicit complete audit of active heads and indexes. Differential tests execute both the structurally shared kernel and the previous deep-clone kernel through deterministic and randomized define/forget/query histories, including full-audit checkpoints.

## Background retirement

Dropping the last foreground `ForthDb` wrapper transfers its internal shared kernel to a bounded `ArrayQueue`. A dedicated reaper thread performs final node destruction.

The reaper records:

- queued roots
- roots retired
- roots reaped
- overflow enqueues
- worker liveness

If the primary queue is full, ownership moves to an overflow queue rather than falling back to synchronous foreground destruction. `ForthDb::drain_reaper()` provides an explicit observation and test boundary.

The current reaper isolates semantic-kernel reclamation. Final database shutdown still releases the world-history spine synchronously; this is recorded separately rather than hidden inside retirement measurements.

## Accepted isolated observation

GitHub Actions run `30783078438` used a release build on a shared Linux x86-64 runner. The reaper queue was drained before every timed candidate and read sample.

### One-definition candidate scaling

| Retained definitions | Shared candidate | Legacy deep clone | Shared allocated bytes |
| ---: | ---: | ---: | ---: |
| 100 | 8.87 µs | 53.53 µs | 41,411 |
| 1,000 | 14.22 µs | 575.18 µs | 59,442 |
| 10,000 | 20.89 µs | 8.64 ms | 79,701 |
| 100,000 | 28.55 µs | not run | 86,312 |
| 1,000,000 | 45.45 µs | not run | 98,150 |

Across a 10,000-fold increase in retained world size, shared candidate latency increased by approximately 5.1 times. The legacy implementation had already reached 8.64 ms at 10,000 definitions.

The shared allocation curve remains bounded below 100 KiB in this observation rather than copying retained world contents.

### Current-head resolution

| Retained definitions | Median resolution |
| ---: | ---: |
| 100 | 22.89 ns |
| 1,000 | 23.73 ns |
| 10,000 | 30.37 ns |
| 100,000 | 34.15 ns |
| 1,000,000 | 35.63 ns |

Current resolution remained in the tens-of-nanoseconds tier and independent of definition-history scans.

### Snapshot retirement

The retirement test held 1,000 snapshots over a 100,000-definition base plus 1,000 successor worlds.

| Metric | Observation |
| --- | ---: |
| Foreground P50 | 231 ns |
| Foreground P95 | 1.02 µs |
| Foreground P99 | 1.15 µs |
| Foreground maximum | 15.61 µs |
| Reaper drain | 637.01 ms |
| Overflow enqueues | 0 |
| Queued roots after drain | 0 |

The destruction cost still exists; it is measured in the background drain rather than charged unpredictably to snapshot-dropping threads.

## Interpretation

Milestone 5 passes the architectural falsification test:

- candidate construction no longer has a linear retained-world slope
- current reads remain fast
- immutable base worlds remain valid after candidate mutation
- foreground reclamation is bounded in the tested workload
- canonical storage and durability implementations remain untouched
- the previous and new semantic kernels remain observationally equivalent under differential testing

These observations are not production guarantees. Remaining hardening includes allocator and memory-residency profiling, reaper failure injection, queue saturation tests, process shutdown policy, crash fault injection, and broader randomized histories.
