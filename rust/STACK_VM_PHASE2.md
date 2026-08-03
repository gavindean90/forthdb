# Phase 2 Layered Indexes and Immutable Roots

## Question

Can the framed stack VM maintain query-equivalent indexes and publish immutable
world roots without heap allocation or renewed wide-epoch collapse?

Phase 2 remains an in-memory experimental kernel. It does not change the
durable admission journal, io_uring transport, recovery decoder, or production
world implementation.

## Index representation

Every accepted visibility transition appends three fixed-size delta entries:

- SPO: subject, predicate, object
- POS: predicate, object, subject
- OSP: object, subject, predicate

Prefix selection over those permutations covers the production kernel's seven
lookup shapes: S, P, O, SP, SO, PO, and exact SPO. Rejected trials restore the
index-delta frontier together with the record, slot-delta, stack, and allocator
frontiers.

The Phase 2 differential reader scans a root's delta prefix and is intended as
a correctness oracle. Building compact searchable base segments and measuring
query latency are subsequent work; the current benchmark measures index
maintenance rather than claiming a complete query engine.

## World roots

Publication appends a POD `WorldRoot` descriptor containing parent identity,
version, record and index frontiers, allocator head, and an incrementally
maintained semantic hash. Backing arenas remain append-only, so an older root
continues to observe exactly its original prefixes after later epochs publish.

The root arena is preallocated. Publication performs no `Arc` allocation or
state copy. Concurrent reader ownership and reclamation are deliberately left
for a later phase.

## Allocation instrumentation

The benchmark installs a counting global allocator. Workspace, program, and
arena capacities are established before the measured stack-VM hot path. It
counts allocations during trial execution, accepted index maintenance, hashing,
rollback, and epoch-root publication. Every Phase 2 width must report zero.

## Differential contract

The test sweep uses widths 16, 64, 128, and 256 and compares:

- accepted and rejected outcomes
- allocator rollback and resulting allocator frontier
- active slot values and `FORGET` unmasking
- older-root immutability after later publication
- S, P, O, SP, SO, PO, and exact query provenance

## Benchmark contract

Every width processes the same 8,192-intent trace; only epoch partitioning
changes. This avoids conflating epoch width with database size. Phase 1 and
Phase 2 execute the same precompiled programs. Phase 2 adds permutation deltas,
incremental semantic hashing, and immutable root publication.

The primary criteria are:

1. width-256 Phase 2 throughput remains at least 80% of width 16
2. measured Phase 2 hot-path allocations remain zero
3. every differential projection and query shape matches the current engine

Meeting these criteria establishes an indexed materialization kernel with adequate
CPU headroom. It does not establish durable database TPS or production query
latency.

The width comparison uses a separate VM-only paired sweep that alternates
measurement order. The full comparison also runs the much slower current engine
and can otherwise bias later cases through hosted-runner frequency and thermal
changes. Correctness and zero allocation are hard assertions; the performance
criterion is reported rather than used as a potentially flaky CI assertion.
