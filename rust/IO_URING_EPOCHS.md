# Milestone 6C: io_uring Durability Transports

> Historical experiment record. The synchronous transports described here
> were slower than the ordinary-file control and have been removed. The only
> current io_uring path is the opt-in speculative controller documented in
> [`SPECULATIVE_IO_URING.md`](SPECULATIVE_IO_URING.md).

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

Only one durability epoch is in flight. A dedicated reactor, multiple epochs in flight, registered files or buffers, `SQPOLL`, and overlap between candidate preparation and durability remain deferred.

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
- no duplicate, unknown, out-of-range, or missing completion identifiers

Any submission error, missing CQE, short write, failed write, failed synchronization, or malformed completion batch discards and recreates the ring before the existing file-epoch repair logic truncates and verifies the known-good checkpoint.

Completion validation is a pure tested boundary. Tests cover out-of-order valid CQEs, missing completions, duplicate writes, unknown and out-of-range identifiers, noncanonical expected identifiers, short writes, negative write results, and negative synchronization results.

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

The dedicated gate ran the ordinary-file per-epoch transport and all three ring strategies through the same durable queued controller over a 100,000-definition base. It tested maximum batch sizes one and sixteen across four rotating rounds, so every transport occupied every warm-up and filesystem-order position once. Every sample reopened through `FileCommitStore` and verified the exact final frame count.

All runs used four producers, ingress capacity 256, a 64-entry ring, cooperative retry after immediate backpressure, and release builds on a shared Linux x86-64 runner.

## Accepted result

GitHub Actions run `30791692940` produced:

| Variant | Max batch | Median batch | Median intents/s | Writes | Syncs | Submits | CQEs | Max writes in flight |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Ordinary per-epoch | 1 | 1.00 | 1,779 | 512 | 512 | 0 | 0 | 0 |
| io_uring contiguous `WRITE` | 1 | 1.00 | 1,631 | 512 | 512 | 512 | 1,024 | 1 |
| io_uring `WRITEV` | 1 | 1.00 | 1,762 | 512 | 512 | 512 | 1,024 | 1 |
| io_uring positional writes | 1 | 1.00 | 1,670 | 512 | 512 | 512 | 1,024 | 1 |
| Ordinary per-epoch | 16 | 16.00 | **8,183** | 128 | 128 | 0 | 0 | 0 |
| io_uring contiguous `WRITE` | 16 | 15.88 | 8,010 | 129 | 129 | 129 | 258 | 1 |
| io_uring `WRITEV` | 16 | 15.88 | 7,978 | 129 | 129 | 129 | 258 | 1 |
| io_uring positional writes | 16 | 15.88 | 7,425 | 2,048 | 129 | 129 | 2,177 | 16 |

The result is negative but decisive:

- ordinary positional write plus `fdatasync` remained the fastest transport in the one-epoch-at-a-time committer
- the contiguous ring variant was about 2 percent slower at batch sixteen
- `WRITEV` eliminated approximately 275 KB of arena copying per round but was about 2.5 percent slower, proving that the copy was not the bottleneck
- true QD=16 worked and was measured honestly, but the additional SQEs and CQE processing made it about 9 percent slower than the ordinary contiguous epoch

The io_uring variants remain valid interoperable transports. They produce the same version-1 bytes, pass the same recovery parser, preserve ticket ordering, and reuse the Milestone 6B repair and poisoning state machine. They are not the default because this experiment found no performance dividend under the tested ownership model.

## Architectural conclusion

A dedicated completion reactor is not justified by this result. Adding a second thread now would introduce another queue, wakeups, buffer-lifetime transfer, and scheduling noise around transports that do not outperform the synchronous ordinary-file control.

The next io_uring experiment, if pursued, must change the concurrency proposition rather than merely the syscall shape—for example, preparing epoch N+1 while epoch N is durable, using registered resources, or testing substantially larger records. Such work requires its own semantic and ownership milestone and must retain the ordinary-file epoch as the control.

## Falsification boundaries

Milestone 6C fails if any transport:

- produces bytes different from `FileCommitStore`
- resolves a successful ticket before CQE and publication verification
- allows an incomplete or failed epoch to advance the reader head
- bypasses verified repair or poisoning
- loses, duplicates, or misattributes a CQE
- represents linked serial writes as queue depth greater than one

All boundaries passed. The performance hypothesis—that io_uring submission shape alone would outperform the ordinary-file epoch—did not.
