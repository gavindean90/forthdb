# Milestone 6D: Lifecycle and Writer Safety

## Purpose

Milestone 6D hardens the accepted one-epoch-at-a-time durability controller before any speculative overlap or multiple-epoch pipeline is introduced.

It does not change queued-intent semantics, canonical frames, world identity, recovery parsing, durability repair, or publication ordering. Its purpose is to make controller ownership, shutdown, process death, and restart behavior explicit and falsifiable.

## Controller lifecycle

The durable controller exposes the following states:

```text
Starting
   ↓
Running
   ↓
Draining ─────► Closed
   │
   └──────────► Poisoned
```

### Starting

The controller is constructing its bounded ingress and worker. Admission is not yet authoritative.

### Running

New queued intents may be admitted subject to fixed-capacity backpressure. The single committer claims intents, derives one private epoch, persists it, publishes the epoch tail, and only then resolves successful tickets.

### Draining

Shutdown has begun. New submissions are rejected immediately.

The worker completes the epoch it has already claimed. Intents still queued behind that claimed epoch are not silently discarded and are not converted into additional shutdown work. Each receives:

```text
DurableTicketOutcome::Stopped(
    DurableTicketStopReason::ShutdownBeforeClaim
)
```

### Closed

The worker has exited normally, admitted tickets have definite outcomes, and any process-scoped writer lease has been released. Repeated shutdown calls are idempotent.

### Poisoned

The worker panicked, exited unexpectedly, or otherwise lost the ability to certify its state. The controller and live file-epoch store reject subsequent writes. Outstanding queued tickets receive a worker-failure outcome where delivery remains possible.

A poisoned controller is not resurrected in place. Restart means acquiring a new writer lease, cold-opening the canonical log, and reconstructing a new controller from recovered durable state.

## Shutdown contract

`shutdown()` returns a `DurableShutdownReport` containing:

- previous and final lifecycle state
- queued intents stopped before claim
- worker-failure count
- poison reason, when present

The contract is:

1. transition `Starting` or `Running` to `Draining`
2. remove the ingress sender so no further admission can succeed
3. let the worker finish its already-claimed epoch
4. resolve queued-but-unclaimed tickets as `ShutdownBeforeClaim`
5. join the worker
6. enter `Closed` unless the worker or store is poisoned
7. release the writer lease only after worker ownership is gone

Dropping a ticket remains independent from intent lifetime. An abandoned ticket does not cancel an admitted intent during normal operation or shutdown.

## Worker failure

The committer boundary catches unwindable worker panics. A panic:

- records a poison reason
- marks the worker dead
- poisons the live file-epoch store
- fences new submissions
- resolves queued tickets as `WorkerFailed` where possible

Tests inject a validator panic while an intent is claimed. The result must be a poisoned controller and store, not a thread that disappears while the public handle still reports health.

Process termination such as `SIGKILL` is handled by cold recovery rather than panic handling.

## Process-scoped writer ownership

Production writable open uses:

```rust
DurableQueuedIntentController::open_owned(...)
```

On Linux this acquires a nonblocking exclusive `flock` on a sidecar path:

```text
<database path>.writer.lock
```

The lock is acquired before mutable store recovery and held through:

- cold open and reconstruction
- normal commits
- repair
- draining
- worker join
- final shutdown

The kernel lock, not the diagnostic contents of the sidecar file, is authoritative. The current process identifier is written only to improve error reporting.

A second writable opener receives `WriterLeaseError::AlreadyHeld`. The operating system releases ownership after process death, including `SIGKILL`; correctness does not depend on Rust `Drop` running.

Non-Linux platforms currently report writer-lease unavailability rather than pretending to provide equivalent exclusion.

## Crash-window semantics

The feature-gated subprocess harness terminates the process with no Rust unwinding at three controller boundaries.

| Crash point | Physical result | Cold recovery |
| --- | --- | --- |
| after derive, before persist | no new frame written | previous durable world |
| after persist, before publish | complete synchronized frame exists | new durable world |
| after publish, before ticket delivery | complete synchronized frame exists | new durable world |

The test then acquires a fresh writer lease, reconstructs the database, commits another intent, and shuts down cleanly.

These tests complement the Milestone 6B byte-boundary and repair tests. Milestone 6B proves behavior during partial write, synchronization failure, repair, and poisoned rollback. Milestone 6D proves the higher controller windows around derivation, publication, delivery, process ownership, and restart.

## Unknown caller outcome

A process may die after the frame is durable but before a ticket reaches its caller. Recovery correctly retains the frame.

Therefore:

> Failure to receive a successful ticket does not prove that the intent failed to commit.

The engine provides truthful at-least-once retry ambiguity at this boundary. Exactly-once application behavior requires a semantic idempotency key or an equivalent committed precondition; it is not inferred from the in-memory ticket channel.

## Timing observability

The durable controller records cumulative nanoseconds for:

- ingress queue wait
- private epoch derivation and validation
- persistence and synchronization
- reader-head publication
- ticket delivery
- complete epoch processing

It also exposes the idealized derive/persist overlap bound:

```text
(derive + persist) / max(derive, persist)
```

The dedicated Milestone 6D observation additionally computes a complete-pipeline upper bound by removing, at most, the smaller of derive and persist from measured epoch time:

```text
full ceiling = epoch_total / (epoch_total - min(derive, persist))
```

This is an optimistic ceiling. It assumes perfect double buffering and adds no reactor, buffer-transfer, scheduling, or speculative-discard overhead.

## Accepted observation

The exact-head lifecycle run used:

- 100,000 retained definitions
- 2,048 accepted intents
- four producers
- ingress capacity 256
- maximum batch 16
- ordinary per-epoch durability

It observed:

| Metric | Result |
| --- | ---: |
| Throughput | 5,350 intents/s |
| Epochs | 129 |
| Average batch | 15.88 |
| Derive per epoch | 2.228 ms |
| Persist per epoch | 0.633 ms |
| Total per epoch | 2.962 ms |
| Core derive/persist ceiling | 1.284× |
| Full-pipeline ceiling | 1.272× |
| Data writes | 129 |
| Data synchronizations | 129 |
| Final canonical frames | 2,049 |

For this workload, candidate derivation dominates physical persistence. A perfect one-epoch speculative pipeline could improve the measured complete epoch path by no more than about 27% before accounting for its own overhead.

This does not rule out larger gains on slower storage, smaller transactions, larger records, or a different machine. It establishes that speculative overlap must be benchmarked as a workload-dependent optimization rather than assumed to be the next universal win.

## Falsification boundaries

Milestone 6D fails if:

- admission succeeds after draining begins
- a queued ticket receives no terminal outcome during shutdown
- shutdown cancels an already-claimed epoch
- a worker panic leaves the public controller apparently healthy
- two processes simultaneously acquire writable ownership
- writer ownership survives `SIGKILL`
- cold recovery loses a synchronized frame
- cold recovery invents a frame that was never persisted
- a restart continues from an in-memory assumption instead of the recovered log
- timing counters are read before a controller barrier and report an incomplete epoch

All listed boundaries pass in the dedicated unit and subprocess harnesses.

## Deferred work

Milestone 6D does not implement:

- speculative derivation of epoch N+1
- multiple durability epochs in flight
- a dedicated completion reactor
- process-independent read leases
- checkpoints or compaction
- true device power-loss simulation
- automatic worker restart inside an existing controller

Any speculative pipeline remains a separate milestone. It must preserve the lifecycle, writer-ownership, recovery, ticket, and poisoning contracts established here, and it must be compared with the ordinary one-epoch controller as its control.
