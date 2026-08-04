# VM-backed immutable world roots

The durable token VM now publishes the authoritative semantic root directly.
It no longer executes an accepted epoch in the VM and then immediately applies
the same operations to a cloned `ForthDb` kernel.

Each published root contains the canonical world identity, version, allocator
frontier, active-slot count, record frontier and one immutable commit frame.
The frame remains the durable and historical compatibility boundary. Tickets,
snapshot acquisition, recovery checks and count observations use the root
without constructing the older query kernel.

## Compatibility queries

The existing `World` API is retained. Calling `resolve`, `definitions`,
`display_name` or `query` materializes a `ForthDb` compatibility projection on
demand. The projection starts from the closest already-materialized ancestor,
applies the remaining immutable frames once and caches the result in the
current `World` with `OnceLock`. A successor may carry a weak pointer to the
nearest live projected base, while history nodes remain plain immutable frame
links. The authoritative history therefore does not pin old query kernels in
memory.

This deliberately moves work rather than hiding it:

- VM publication and recovery do not pay for a reader representation that may
  never be used.
- A legacy query pays a one-time projection cost for that immutable root.
- Later queries reuse the cached projection.
- A successor can reuse the closest projected ancestor instead of replaying
  from genesis.

`World::is_query_projection_materialized()` and
`World::materialize_query_projection()` make the boundary observable to tests
and benchmarks.

## Semantic invariants

- The canonical admission journal is unchanged.
- The VM still evaluates every intent in order and rolls rejected trials back
  to POD frontiers.
- Accepted intents in one admission epoch observe the same published world.
- The VM and compatibility projection calculate the same frame and world ID.
- Recovery recompiles the durable journal through the VM before publishing its
  final root.
- Registered host validators retain the eager world-materializer fallback.

This milestone does not yet make the compact SPO/POS/OSP snapshot the complete
public query engine. It removes eager double execution and measures the actual
cost of retaining the established query API. Replacing that lazy compatibility
projection with direct VM-index result materialization remains a separate,
falsifiable step.
