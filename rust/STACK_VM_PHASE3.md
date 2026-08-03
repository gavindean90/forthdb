# Phase 3 Compact Bases and Reader Snapshots

## Question

Can readers query immutable stack-VM worlds without scanning an ever-growing
delta history, and can a materializer publish new reader ownership without a
lock?

Phase 3 remains an in-memory experiment. It does not change the admission
journal, recovery format, or production `World` implementation.

## Compact query bases

Compaction selects the visible SPO records at an older `WorldRoot` and builds
three contiguous, sorted POD arrays:

- SPO: subject, predicate, object
- POS: predicate, object, subject
- OSP: object, subject, predicate

The seven supported query shapes select a permutation and use binary prefix
bounds. A `LayeredSnapshot` owns the compact base through `Arc<CompactBase>` and
copies only the immutable index-delta suffix needed to reach its root. Queries
search the base and then reconcile that short tail.

Compaction allocates and scans the selected root's SPO delta history. It is a
checkpoint operation outside the materialization hot path, not an O(1)
publication claim.

## Reader ownership

The benchmark wraps complete `LayeredSnapshot` values in `Arc` and publishes
them through `ArcSwap`. A reader atomically acquires either the old complete
snapshot or the new complete snapshot; it cannot observe a base and tail from
different worlds. Existing readers retain their old snapshot without locking
the writer.

`Arc` alone would only make an already-acquired snapshot shareable. The atomic
swap is the part that provides lock-free acquisition of the currently
published root.

## Differential contract

The existing width sweep now also compacts its first root, layers the remaining
deltas to the final root, and compares S, P, O, SP, SO, PO, and exact SPO query
results with the production world. This includes repeated definition,
rejection, and `FORGET` unmasking.

The concurrent benchmark alternates two valid immutable snapshots while four
readers execute point and predicate-range counts. Every observed result must
belong to one complete snapshot.

## Benchmark scope

The report measures:

- exact point-count latency and QPS
- predicate-range count latency and QPS
- mixed point/range QPS while snapshots are atomically replaced
- compact-base fact count and uncompacted tail size

These are in-memory index-kernel measurements over 8,192 intents, not durable
database query numbers. Range counts do not materialize result rows, and the
experiment does not yet define checkpoint cadence, background compaction,
memory reclamation, durable token integration, or a production reader API.
