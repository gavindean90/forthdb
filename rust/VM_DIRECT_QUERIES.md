# Direct immutable-root queries

VM-backed `World` snapshots now answer the established read API without first
constructing a legacy `ForthDb`. The query projection is derived directly from
canonical immutable commit frames and cached once per queried root.

The projection preserves each slot's visible definition stack and builds
compact, record-ordered `Vec` buckets for the seven existing access paths:
subject, predicate, object, subject/predicate, subject/object,
predicate/object and exact SPO. Define pushes a visible definition; forget
pops it and reveals the predecessor. Entity-allocation operations do not enter
the read projection.

This keeps the public semantics unchanged:

- resolve and definitions retain redefine/forget behavior;
- display names use the same reserved display slots;
- joins and repeated variables use the same binding rules;
- optimizer path selection sees the same exact candidate counts;
- limits, distinct rows, provenance and deterministic result ordering match
  the legacy kernel.

The projection is not raw process memory. Its first construction still walks
history from the current root, after which `OnceLock` makes reads reuse the
immutable indexed view. [`SEMANTIC_CHECKPOINTS.md`](SEMANTIC_CHECKPOINTS.md)
persists the accepted semantic frame prefix so recovery can skip intent replay;
the read indexes remain a derived in-memory view.

`World::is_query_projection_materialized()` observes the native read view.
`World::is_legacy_query_projection_materialized()` separately proves whether
the compatibility kernel was needed. Normal VM-backed application queries
should leave the latter false.
