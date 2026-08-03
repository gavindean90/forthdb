# Durable Admission Epochs and Immutable Worlds

## Identity

ForthDB is an embedded durable intent journal and deterministic semantic
materializer. Ordered admission epochs become immutable queryable worlds.

The journal is authoritative. A world is the deterministic semantic projection
of the sound durable epoch prefix.

## Pipeline

```text
queued intents
    -> canonical admission epoch
    -> io_uring WRITE + DATASYNC
    -> durable admission receipts
    -> deterministic semantic evaluation
    -> one immutable epoch world
    -> accepted or rejected outcomes
```

The worker may submit durability for epoch N+1 before materializing epoch N.
The pending io_uring request retains ownership of its arena until every CQE is
reaped, including when semantic evaluation panics.

## Ticket truth

An admission ticket exposes two different facts:

- `wait_admitted` proves that the ordered intent and its epoch boundary are in
  the durable journal.
- `wait` reports whether deterministic semantic evaluation accepted or
  rejected that intent and identifies the published epoch world.

Durable admission is not semantic success. Rejected intents remain in the
journal but consume no semantic state or entity allocation.

## Epoch worlds

Intents are evaluated in order against a private evolving candidate. Accepted
operations are collapsed into one canonical frame rooted at the preceding
published world. Every accepted intent in the admission epoch observes the same
world identity. Intermediate private candidates are never published.

An all-rejected epoch advances the applied journal watermark without creating
a world.

## Recovery

The journal uses checksummed, length-delimited records with durable epoch IDs.
An incomplete trailing record is truncated to the last sound boundary. Replay
decodes the exact admitted intents and reconstructs one world per epoch that
contains an accepted effect.

Because the journal stores intentions rather than post-validation frames, any
validator that affects acceptance must be supplied to `open_with_validators`
before recovery. Time, randomness, external observations and other
nondeterministic inputs must be captured as intent data before admission.

Outcome summaries and checkpoints may later accelerate recovery, but they are
not required for correctness.

Client-supplied idempotency keys and deduplication across process retries are
not implemented in this first controller and remain required before treating
durable admission as a production ingestion API.

## Backpressure

Fast durable admission does not remove the semantic throughput ceiling. The
controller reports durable and applied epochs separately and measures maximum
semantic lag. A production policy must bound this lag rather than allowing the
journal to outrun materialization indefinitely.

## Library reference

The Rust library application groups seven domain intentions into five durable
epoch worlds:

1. stable entity allocation
2. metadata and catalog construction
3. checkout and independent relocation
4. rename and symbol rebinding
5. return

The application observes durable admission before semantic acceptance,
preserves historical snapshots and compiled identity, then closes and reopens
the admission journal to prove reconstruction of the identical final world.

The ramped library reference uses the same model at application scale: 10,000
works, 20,000 copies, 5,000 patrons, eight branches, and a deterministic mix of
checkout, contention, holds, moves, loss/recovery, renames, and returns. It runs
the identical ordered trace with one-intent interactive epochs and 16-intent
branch-rush epochs. The comparison treats equal final projections and exact
per-profile recovery as correctness gates, then reports throughput, admission
and semantic latency, syncs per intent, semantic lag, query latency, and history
growth.
