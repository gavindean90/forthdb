# Milestone 6B: Ordinary-File Durability Epochs

## Purpose

Milestone 6B asks whether ForthDB can synchronize several independently meaningful canonical commit frames with one ordinary-file durability barrier while preserving exact history, reader visibility, ticket truthfulness, and recoverability.

It deliberately uses synchronous ordinary-file I/O. `io_uring`, `WRITEV`, drained barriers, dwell-time batching, and adaptive policy remain deferred to later stages.

## Paired transport control

`FileEpochStore` accepts the same ordered frame slice under two policies:

```text
PerFrame:
    write F1 -> fdatasync
    write F2 -> fdatasync
    ...
    write Fk -> fdatasync

PerEpoch:
    encode [F1 F2 ... Fk] into one contiguous arena
    write arena -> fdatasync
```

Both policies write the existing version-1 `FRM1 ... END1` records. They produce byte-identical files and recover through the established `FileCommitStore` parser.

Batch size one is the no-amortization control: both policies perform one write and one synchronization per accepted intent. At larger batches, only the synchronization policy differs.

## Durable controller ordering

`DurableQueuedIntentController` retains the bounded admission, claim, ticket-abandonment, and result-routing contract established in Milestone 6A.2.

For one worker batch:

```text
claim admitted intents
        ↓
derive private successor chain
        ↓
persist every accepted CommitFrame
        ↓
verify durability completion
        ↓
advance global reader head once to the epoch tail
        ↓
resolve each ticket independently
```

An accepted ticket cannot resolve before the file epoch succeeds and the reader head has advanced.

Semantic rejection remains independent. A rejected intent produces no frame and retains its typed rejection result even when a neighboring accepted intent encounters a durability failure.

On an observed epoch failure:

- no accepted world is published
- every semantically accepted member receives `DurabilityFailed`
- semantically rejected members remain rejected
- the store repairs before another epoch is attempted
- ticket abandonment does not cancel repair or history processing

## File I/O boundary

Production and tests share one narrow interface:

```text
len
positional write
fdatasync
truncate
read all
```

Every operation carries a semantic phase:

- `EpochStart`
- `EpochWrite`
- `EpochSync`
- `RepairTruncate`
- `RepairSync`
- `VerifyLength`
- `VerifyRead`

`StdEpochFileIo` delegates directly to a real `std::fs::File`. Fault tests wrap the same real file and alter only the requested outcome at a named phase. The canonical encoder, checkpoint logic, repair state machine, parser, and physical bytes remain production code.

## Checkpoint and repair contract

Before writing an epoch, the store records:

- physical start offset
- verified frame count
- verified tail `WorldId`
- verified tail version
- digest of the complete known-good prefix

The live state machine is:

```text
Healthy
   ↓
Writing epoch
   ├── success -> Healthy
   └── observed failure -> Repairing
                              ├── exact verified rollback -> Healthy
                              └── uncertainty or double fault -> Poisoned
```

Repair requires every step to succeed:

1. truncate to the epoch start offset
2. synchronize the truncation
3. verify exact physical length
4. read the real file without mutating it
5. verify the prefix digest
6. parse the entire file with no trailing bytes
7. verify frame count and tail identity/version
8. reconstruct the complete world history

A length match alone is insufficient.

## Poisoning discipline

Any failed or uncertain repair permanently poisons the live store handle.

A poisoned handle rejects:

- all later appends
- physical frame reads
- physical length queries used as trusted store state

It may still return the cached frames that were verified before poisoning. Those frames support already-published immutable worlds; poisoning the physical tail does not retroactively invalidate known-good in-memory snapshots.

Cold restart is a separate proof boundary. The normal `FileCommitStore::open` path may discard an incomplete trailing record and synchronize that truncation. A complete malformed frame, checksum failure, or nonlinear history remains a hard corruption error.

## Deterministic fault matrix

The test suite uses real temporary files and injects:

- failure before an epoch write
- a real prefix write followed by `ENOSPC`
- a complete arena write followed by epoch-sync `EIO`
- repair-truncate `EIO`
- truncation that physically succeeds but reports `EIO`
- repair-sync `EIO`
- verification-length mismatch

Successful repair must restore byte-for-byte checkpoint equality and permit the next epoch.

Every double fault must poison the handle, and every later write or physical read must fail immediately.

## Every-byte interruption sweep

For a real three-frame epoch, the harness injects a prefix write followed by `ENOSPC` at every byte boundary from zero through the complete arena length.

Every case must:

- publish no frame
- return the store to `Healthy`
- restore the exact pre-epoch bytes
- reopen as the exact pre-epoch history

This challenges frame magic, length, checksum, payload, trailer, frame boundaries, and the exact end of the arena.

## Process-crash recovery

A subprocess appends real frame bytes and terminates with `process::exit`, bypassing Rust unwinding and `Drop` cleanup.

The parent then opens the file through production recovery:

- a partial final frame is removed and only the longest sound prefix is exposed
- a complete final frame is retained and exposed

These tests establish process-crash byte-pattern recovery. They do not claim to simulate device-cache persistence after physical power loss.

## Accepted paired benchmark

GitHub Actions run `30788819660` used release builds on a shared Linux x86-64 runner. Each policy/batch configuration ran three times, with run order alternating to control warm-up and filesystem-order effects.

All arms used:

- 100,000 retained definitions
- four producer threads
- ingress capacity 256
- 10 µs cooperative retry after immediate backpressure
- the same intent construction, bounded ingress, ticket delivery, candidate derivation, publication, frame encoding, and recovery checks

| Policy | Max batch | Intents/round | Median batch | Median intents/s | Median syncs | Syncs/intent | Throughput range |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Per-frame | 1 | 512 | 1.00 | 2,502 | 512 | 1.0000 | 2,054–2,523 |
| Per-epoch | 1 | 512 | 1.00 | 2,494 | 512 | 1.0000 | 2,399–2,578 |
| Per-frame | 16 | 2,048 | 16.00 | 2,620 | 2,048 | 1.0000 | 2,225–2,626 |
| Per-epoch | 16 | 2,048 | 16.00 | 6,541 | 128 | 0.0625 | 6,306–10,672 |

At batch size one, the policies converged within approximately 0.3 percent, as expected when both issue the same number of writes and synchronizations.

At batch size sixteen, one-sync epochs:

- reduced synchronizations by 16 times
- raised median complete-pipeline throughput by approximately 2.50 times
- recovered all 2,049 frames after every measured run

The throughput gain is therefore attributable to durability amortization rather than a different semantic or queueing path.

## Boundaries preserved

Milestone 6B does not change:

- `CommitFrame`
- version-1 frame bytes
- world identity
- queued-intent predecessor semantics
- temporary-entity scoping
- semantic rejection behavior
- stale strict-transaction behavior
- `FileCommitStore` or `MmapCommitStore`

It adds a new ordinary-file epoch transport and a durable controller around the existing semantic planner.

## Subsequent experiment

Synchronous io_uring submission shapes did not beat this ordinary-file
control and have been retired. The remaining opt-in experiment instead uses
io_uring to overlap durability of epoch N with private preparation of epoch
N+1; see [`SPECULATIVE_IO_URING.md`](SPECULATIVE_IO_URING.md).

The third form is the true queue-depth experiment. Linking all writes to one another would serialize them and is not an acceptable QD > 1 control.
