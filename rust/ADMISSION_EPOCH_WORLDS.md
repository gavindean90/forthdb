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
semantic lag. `open_with_window` configures the maximum number of durable but
unapplied epochs; `open` retains the conservative one-epoch default. Once the
window is full, further durability progresses only as semantic materialization
advances the applied watermark. A production policy must derive this bound from
acceptable semantic staleness, recovery time, journal capacity, and outstanding
ticket memory rather than allowing the journal to outrun materialization
indefinitely.

The `admission_window` benchmark separates three observations:

1. a gated journal-ceiling phase fills the configured window while semantic
   publication is deliberately paused
2. a steady-state phase runs admission and publication concurrently
3. a catch-up phase measures the interval from the final durable receipt to the
   final semantic outcome

The benchmark sweeps unapplied windows of 1, 2, 8, and 32 epochs at 16 intents
per epoch, plus epoch widths of 1, 64, 128, and 256 at an eight-epoch window.
During the gated phase it asserts that the reader-visible world remains
unchanged.

## Materialization research

[`STACK_VM_PHASE1.md`](STACK_VM_PHASE1.md) defines an isolated in-memory
experiment for replacing cloned trial worlds with a framed operand stack, POD
record arenas, and private trial deltas. It intentionally does not change this
journal or its recovery contract. Its benchmark ratio covers a reduced
slot-head projection and must not be reported as complete database throughput.

[`STACK_VM_PHASE2.md`](STACK_VM_PHASE2.md) layers POD SPO/POS/OSP delta streams,
incremental semantic hashing, and immutable POD world-root descriptors over the
same VM. It keeps the trace size constant across epoch widths and treats zero
hot-path allocation plus parity across all seven query shapes as correctness
gates. Compacted query bases and concurrent reader ownership remain subsequent
work.

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
