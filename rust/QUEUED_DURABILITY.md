# Milestone 6: Queued Durability

## Purpose

Milestone 6 asks whether ForthDB can amortize one durability barrier across multiple independently meaningful commit frames without changing strict transaction semantics, canonical history, reader visibility, or failure truthfulness.

Milestone 6A implements only the semantic control. It introduces no background committer, dwell timer, file epoch, `WRITEV`, or deeper io_uring queue.

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

## Canonical and differential controls

Milestone 6A tests:

- private temporary-entity namespaces
- rejection without allocator or version consumption
- predecessor-relative value and absence checks
- per-intent validator rejection
- exact world and frame parity with strict sequential execution
- byte-for-byte `FileCommitStore` parity
- one-tail in-memory publication
- unchanged strict stale-writer behavior
- a deterministic 10,000-intent differential sequence

The committed frame remains the smallest durable history unit. An epoch is not a compound transaction and does not change version 1 encoding.

## History lifecycle finding

The 10,000-intent test exposed recursive destruction of a uniquely owned `HistoryNode` spine. ForthDB now dismantles unique history chains iteratively. Shared ancestors remain reference-counted and are released by their eventual last owner.

This changes reclamation mechanics, not world identity, frame bytes, or recovery.

## Accepted semantic observation

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

Private epoch cost remains approximately proportional to the accepted-intent count. Per-intent derivation stayed in roughly the 25–39 µs range and did not degrade as the chain length increased. Larger epochs amortized fixed planner overhead but did not remove the single-committer CPU cost.

This establishes a semantic control, not a durability-throughput result. Stage 6B must compare the cost of one synchronized epoch against the same frames committed through separate synchronization barriers.

## Stage boundaries

### 6A — semantic control

- pure queued-intent derivation
- temporary entity resolution
- predecessor-relative preconditions
- per-intent rejection
- one-tail in-memory publication

### 6A.2 — ingress and tickets

- bounded staged queue
- ticket lifecycle
- ticket abandonment without cancellation
- explicit claim boundary
- no background durability yet

### 6B — ordinary file epoch

- contiguous canonical frame arena
- one write plus one synchronization
- repairing and poisoned store states
- truncate, synchronize, reopen, and verify before reuse
- crash-point valid-prefix tests

### 6C — io_uring transports

- contiguous `WRITE` plus linked `FSYNC(DATASYNC)`
- `WRITEV` plus linked synchronization
- independent positional writes followed by a drained synchronization barrier

### 6D — policy sweep

- batch size
- encoded-byte cap
- dwell time
- ingress backpressure
- latency and throughput distributions

## Deferred semantics

Milestone 6A does not claim exactly-once execution. A caller interrupted around durability may have an unknown outcome. Idempotency requires an application-level committed identifier or a future canonical protocol addition.

Milestone 6A also does not make an epoch crash-atomic as a whole. Future durable epochs continue to consist of independently valid canonical frames, so recovery may expose the longest sound frame prefix after a process crash.
