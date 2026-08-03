# Milestone 6C: io_uring Durability Transports

## Purpose

Milestone 6C asks which Linux io_uring submission shape best executes the ordinary-file durability epoch contract proved in Milestone 6B.

It does not change queued-intent semantics, canonical frames, recovery rules, publication ordering, or the `Healthy -> Repairing -> Healthy | Poisoned` state machine.

## Completion ownership

The existing committer thread remains the sole ring owner. For one epoch it:

1. derives and validates the private successor chain
2. supplies the accepted canonical frame records to the selected transport
3. submits one complete epoch
4. waits for and validates every required CQE
5. returns to the established repair state machine on any failure
6. publishes the epoch tail and resolves tickets only after success

Only one durability epoch is in flight. A dedicated reactor, multiple epochs in flight, registered files or buffers, `SQPOLL`, and overlap between candidate preparation and durability are deferred until this isolated transport comparison produces evidence that they are worthwhile.

## Transport matrix

### A. Contiguous write

```text
copy [F1 ... Fk] into one arena
WRITE(arena) + IO_LINK
FSYNC(DATASYNC)
```

This is the direct io_uring equivalent of the accepted Milestone 6B ordinary-file control.

### B. Vectored write

```text
iovec[F1, F2, ... Fk]
WRITEV(iovec) + IO_LINK
FSYNC(DATASYNC)
```

The frame records remain independently allocated and alive through completion. This avoids the contiguous arena copy while adding an iovec table and scatter-gather processing.

### C. Pipelined positional writes

```text
WRITE(F1, offset1) ┐
WRITE(F2, offset2) ├─ independent SQEs
...                │
WRITE(Fk, offsetk) ┘
FSYNC(DATASYNC) + IO_DRAIN
```

The writes are deliberately not linked to one another. `IO_DRAIN` on the synchronization request forms the completion-ordering barrier after all prior writes while preserving true write queue depth.

## Buffer and CQE contract

All arenas, frame records, and iovec arrays remain alive until the committer has reaped and validated every expected CQE.

A successful epoch requires:

- exactly one completion for every submitted write
- each write result to equal its expected byte length
- exactly one synchronization completion
- synchronization result zero
- no duplicate, unknown, or missing completion identifiers

Any submission error, missing CQE, short write, failed write, or failed synchronization discards and recreates the ring before the existing file-epoch repair logic truncates and verifies the known-good checkpoint.

## Metrics

The transport reports:

- data writes
- durability synchronizations
- bytes written
- submission calls
- completion events
- maximum writes in flight
- iovecs submitted
- bytes copied into a contiguous arena

These metrics are accumulated by `FileEpochStore` beside the established repair and commit counters.

## Benchmark control

The dedicated gate runs the ordinary-file per-epoch transport and all three ring strategies through the same durable queued controller over a 100,000-definition base. It tests maximum batch sizes one and sixteen across four rotating rounds, so every transport occupies every warm-up and filesystem-order position once. Every sample reopens through `FileCommitStore` and verifies the exact final frame count.

Batch size one is the no-amortization control. At batch size sixteen the transport metrics distinguish one arena write, one vectored write, and true multi-write queue depth while preserving one durability barrier per epoch.

## Falsification boundaries

Milestone 6C fails if any transport:

- produces bytes different from `FileCommitStore`
- resolves a successful ticket before CQE and publication verification
- allows an incomplete or failed epoch to advance the reader head
- bypasses verified repair or poisoning
- loses, duplicates, or misattributes a CQE
- represents linked serial writes as queue depth greater than one

The benchmark must compare all three ring variants against the same ordinary-file per-epoch controller and include a batch-size-one no-amortization control.
