# VM-backed immutable world roots

The durable token VM now publishes the authoritative semantic root directly.
It no longer executes an accepted epoch in the VM and then immediately applies
the same operations to a cloned `ForthDb` kernel.

Each published root contains the canonical world identity, version, allocator
frontier, active-slot count, record frontier and one immutable commit frame.
The frame remains the durable and historical compatibility boundary. Tickets,
snapshot acquisition, recovery checks and count observations use the root
without constructing the older query kernel.

## Direct queries

The existing `World` API is retained. Calling `resolve`, `definitions`,
`display_name` or `query` on a VM-backed root materializes a native indexed
projection directly from immutable frames. It preserves visible definition
stacks and record-ordered buckets for all seven SPO access paths, then caches
that projection in the current `World` with `OnceLock`. It does not construct a
`ForthDb` compatibility kernel. See
[`VM_DIRECT_QUERIES.md`](VM_DIRECT_QUERIES.md).

This deliberately moves work rather than hiding it:

- VM publication and recovery do not pay for a reader representation that may
  never be used.
- The first query pays a one-time native projection cost for that immutable root.
- Later queries reuse the cached projection.
- A future checkpoint can provide a compact base without changing query
  semantics.

`World::is_query_projection_materialized()`,
`World::is_legacy_query_projection_materialized()` and
`World::materialize_query_projection()` make both boundaries observable.

## Semantic invariants

- The canonical admission journal is unchanged.
- The VM still evaluates every intent in order and rolls rejected trials back
  to POD frontiers.
- Accepted intents in one admission epoch observe the same published world.
- The VM and direct query projection expose the same facts as the compatibility
  kernel.
- Recovery recompiles the durable journal through the VM before publishing its
  final root.
- Registered host validators retain the eager world-materializer fallback.

The projection is still rebuilt from journal-derived history after process
recovery. Persisted compact checkpoints remain a separate, falsifiable step.
