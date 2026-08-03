# Milestone 6: Queued Durability

## Purpose

Milestone 6 asks whether ForthDB can amortize one durability barrier across multiple independently meaningful commit frames without changing strict transaction semantics, canonical history, reader visibility, or failure truthfulness.

Milestones 6A and 6A.2 establish the semantic and concurrency controls. They introduce no file epoch, durability batching, dwell timer, `WRITEV`, or deeper io_uring queue.

## Strict transactions and queued intents

A `Transaction` has one absolute base world. `Database::commit(transaction)` retains the existing stale-writer rule:

```text
transaction.base_world == current_world
```

A `QueuedIntent` explicitly delegates predecessor assignment to the epoch planner. It is not automatically rebased from a strict transaction.

Strict commits and in-memory queued epochs share the same commit serialization lock. Whichever advances the head first determines whether an older strict transaction is stale.

## Temporary entities

A queued intent allocates opaque `TempEntity` handles. Each handle contains an internal per-intent namespace plus an intent-local index.

```text
Intent A / TempEntity(0) -> EntityId(500)
Intent B / TempEntity(0) -> EntityId(501)
```

A temporary handle copied from one intent cannot alias an equally numbered handle in another intent. Resolution occurs only after the planner assigns the intent a private predecessor world.

Temporary mappings are discarded when an intent is rejected. Rejection therefore consumes no permanent allocator state.

## Preconditions

Milestone 6A supports predecessor-relative:

- expected world identity
- expected exact slot value
- expected slot absence

Preconditions are checked against the private predecessor assigned to the intent, not against the global head observed when the producer constructed it.

An opaque record-head identity precondition is not yet exposed because the public world API does not currently publish record identifiers. Milestone 6A does not pretend fact equality is equivalent to record-head identity.

## Pure epoch derivation

`derive_epoch(base, intents, validators)` processes intents in ingress order:

```text
private predecessor
        ↓
check preconditions
        ↓
resolve temporary entities
        ↓
construct and validate candidate
   ┌────┴────┐
accepted   rejected
   │           │
append Wᵢ/Fᵢ   publish no frame
   │           │
new predecessor predecessor unchanged
```

Rejected intents:

- produce no world
- produce no `CommitFrame`
- consume no world version
- consume no entity identifier
- do not disturb the predecessor assigned to the next intent

The resulting `EpochPlan` contains outcomes in original ingress order, accepted canonical frames, every accepted intent's precise immutable world, and the final tail world.

Derivation is pure with respect to the supplied base and global database head.

## Stage 6A publication control

`Database<MemoryCommitStore>::commit_queued_epoch()` is an infallible semantic control:

1. Acquire the same advancement lock used by strict commits.
2. Snapshot the current world and validator set.
3. Derive the complete private successor chain.
4. Append every accepted canonical frame to `MemoryCommitStore`.
5. Advance the global reader head once to the epoch tail.
6. Return each accepted or rejected outcome in ingress order.

Readers of the global head observe the pre-epoch world or the epoch tail. Accepted intermediate worlds remain available through their individual outcomes and through committed history.

This method is intentionally specialized to the infallible memory store. A fallible durable store needs the repair and poisoning state machine specified for Milestone 6B.

## Stage 6A.2 bounded ingress

`QueuedIntentController` adds one background in-memory committer around the proven Stage 6A control.

The ingress is a fixed-capacity standard-library multi-producer/single-consumer channel. It is chosen as a correctness control, not claimed to be a lock-free final transport.

`submit(intent)` reserves a bounded admission slot before exposing the command to the worker and uses nonblocking `try_send` semantics:

```text
capacity available -> return CommitTicket
capacity exhausted -> return SubmitError::Full(original_intent)
worker unavailable -> return SubmitError::Closed(original_intent)
```

No candidate world is constructed before successful admission. The admission counter is reserved with compare-and-swap, released exactly when the worker claims the item, and tested under fast and concurrent claim races. Its observed maximum cannot exceed configured capacity.

The worker blocks for the first intent and drains only the burst already present, up to `max_batch`. There is no dwell timer and no attempt to wait for a fuller batch under sparse load.

Cross-producer ordering is the order accepted by the MPSC channel. Ordering within one producer follows channel submission order.

## Ticket lifecycle and abandonment

A `CommitTicket` tracks two independent properties:

```text
phase: Queued -> Claimed -> Resolved
abandoned: false -> true
```

Dropping an unobserved ticket sets only the abandonment bit. The worker never consults that bit during derivation or publication.

Therefore abandonment:

- does not remove a queued intent
- does not cancel a claimed intent
- does not change epoch order
- does not reclaim an allocator reservation, because permanent allocation occurs only during derivation
- does not prevent the accepted frame from entering history

After publication, the worker attempts to send each result. If the ticket receiver no longer exists, the failed delivery is counted and discarded. The committed transition remains authoritative.

Tests cover abandonment both before and after claim. The pre-claim test blocks the predecessor, drops the second ticket while it is still queued, then proves that the second intent is later claimed, committed, and recorded as one failed completion delivery.

## Ticket resolution and reader visibility

For every worker batch:

1. Mark admitted items claimed.
2. Invoke `commit_queued_epoch()`.
3. Wait for all accepted frames to enter the memory control and for the global head to advance once to the tail.
4. Resolve each ticket independently.

An accepted ticket therefore cannot resolve before tail publication. It receives its exact causal `Arc<World>`, canonical frame, and temporary-entity mapping.

A rejected ticket receives a typed `TicketRejection` summary. Rejection affects only its own result and does not consume a version or allocator identifier.

`flush()` is an administrative ordering barrier: it waits until commands submitted before the barrier have been processed. It is not a durability operation.

## Observability

The controller exposes:

- successful admissions
- immediate backpressure events
- claimed intents
- accepted and rejected intents
- epochs processed
- abandoned tickets
- failed completion deliveries
- current and maximum queue depth
- current in-flight batch size
- worker liveness

If the worker terminates, later submissions return `Closed`, unresolved ticket receivers disconnect, and worker liveness becomes false. Stage 6A.2 does not yet implement worker restart.

## Canonical and differential controls

Milestones 6A and 6A.2 test:

- private temporary-entity namespaces
- rejection without allocator or version consumption
- predecessor-relative value and absence checks
- per-intent validator rejection
- exact world and frame parity with strict sequential execution
- byte-for-byte `FileCommitStore` parity
- one-tail in-memory publication
- unchanged strict stale-writer behavior
- a deterministic 10,000-intent differential sequence
- eight concurrent producers with 2,000 admitted intents
- exact one-to-one ticket/version accounting under contention
- immediate bounded backpressure
- abandonment before and after claim

The committed frame remains the smallest durable history unit. An epoch is not a compound transaction and does not change version 1 encoding.

## History lifecycle finding

The 10,000-intent test exposed recursive destruction of a uniquely owned `HistoryNode` spine. ForthDB now dismantles unique history chains iteratively. Shared ancestors remain reference-counted and are released by their eventual last owner.

This changes reclamation mechanics, not world identity, frame bytes, or recovery.

## Stage 6A semantic observation

GitHub Actions run `30785402013` used a release build on a shared Linux x86-64 runner. Each observation timed one complete private epoch over a 100,000-definition base. Intent construction occurred before timing; every intermediate world remained alive until the timer stopped; semantic-kernel reclamation was drained before and after every epoch.

| Accepted intents | Median epoch | Median per intent | P95 per intent |
| ---: | ---: | ---: | ---: |
| 1 | 38.71 µs | 38.71 µs | 43.20 µs |
| 2 | 64.29 µs | 32.15 µs | 35.99 µs |
| 4 | 121.22 µs | 30.30 µs | 37.50 µs |
| 8 | 221.64 µs | 27.70 µs | 31.44 µs |
| 16 | 438.90 µs | 27.43 µs | 34.74 µs |
| 32 | 796.00 µs | 24.88 µs | 25.68 µs |
| 64 | 1.63 ms | 25.54 µs | 26.26 µs |

Private epoch cost remains approximately proportional to accepted-intent count. Per-intent derivation stayed in roughly the 25–39 µs range and did not degrade as the chain length increased.

## Stage 6A.2 integrated observation

GitHub Actions run `30786722535` used a release build on a shared Linux x86-64 runner. The controller operated over a 100,000-definition base with capacity 256 and maximum batch 16. Producers paused for 10 µs after immediate backpressure rather than hot-spinning. Successful tickets were consumed as they resolved, so the harness did not manufacture a completion backlog.

| Workload | Intents | Epochs | Average batch | Intents/s | Backpressure |
| --- | ---: | ---: | ---: | ---: | ---: |
| One producer | 1,024 | 65 | 15.75 | 6,329 | 1,076 |
| Four producers | 4,096 | 257 | 15.94 | 7,318 | 17,203 |
| Eight producers | 8,192 | 512 | 16.00 | 11,080 | 42,034 |
| Four producers, abandoned tickets | 4,096 | 256 | 16.00 | 7,309 | 17,018 |

The abandoned workload recorded all 4,096 tickets as abandoned and all 4,096 completion sends as failed, while all 4,096 intents still committed.

The controller maintained near-full batches under sustained load and lost or duplicated no admitted intent. The integrated throughput is materially below the isolated planner ceiling because it includes admission contention, intent construction, commit-lock acquisition, memory-store publication, head advancement, per-ticket channels, result routing, and reclamation effects.

These are observations, not merge thresholds or durability-throughput claims. Stage 6B must benchmark the complete controller plus file epoch against the same controller performing one synchronization per frame; adding an fdatasync estimate to the isolated Stage 6A planner number would be misleading.

## Stage boundaries

### 6A — semantic control — implemented

- pure queued-intent derivation
- temporary entity resolution
- predecessor-relative preconditions
- per-intent rejection
- one-tail in-memory publication

### 6A.2 — ingress and tickets — implemented

- bounded MPSC ingress
- immediate backpressure with intent ownership returned
- explicit queued, claimed, and resolved phases
- ticket abandonment without cancellation
- independent result routing
- observable worker and queue state
- no background durability

### 6B — ordinary file epoch

- contiguous canonical frame arena
- one positional write plus one synchronization
- repairing and poisoned store states
- truncate, synchronize, reopen, and verify before reuse
- crash-point valid-prefix tests
- paired control using one synchronization per frame

### 6C — io_uring transports

- contiguous `WRITE` plus linked `FSYNC(DATASYNC)`
- `WRITEV` plus linked synchronization
- independent positional writes followed by a drained synchronization barrier

### 6D — policy sweep

- batch size
- encoded-byte cap
- dwell time
- ingress backpressure policy
- latency and throughput distributions

## Deferred semantics

Milestone 6 does not claim exactly-once execution. A caller interrupted around durability may have an unknown outcome. Idempotency requires an application-level committed identifier or a future canonical protocol addition.

Milestone 6 also does not make an epoch crash-atomic as a whole. Future durable epochs continue to consist of independently valid canonical frames, so recovery may expose the longest sound frame prefix after a process crash.
