# Semantic checkpoints

The token-VM admission controller can persist a derived semantic checkpoint
without replacing the canonical admission journal. A checkpoint records:

- the exact durable admission-epoch count and journal byte boundary;
- a digest of the complete journal prefix at that boundary;
- the authoritative world identity, version and allocator frontier;
- the canonical accepted commit frames needed to restore immutable history and
  rebuild the token VM without re-evaluating admitted intents.

Checkpoint creation first crosses the controller barrier, so durability and
semantic publication have converged. It writes a versioned, length-delimited,
checksummed temporary file, synchronizes it, atomically renames it over the
previous checkpoint and synchronizes the containing directory.

## Recovery

On open, ForthDB accepts a checkpoint only when its header, length, checksum,
trailer, journal boundary digest, frame chain, world identity, version and
allocator all agree. It restores the immutable VM root and token materializer
from the accepted operations, then decodes and executes only admission epochs
after the checkpoint boundary.

A missing, stale, truncated, corrupt or semantically inconsistent checkpoint
is ignored. Recovery then follows the established complete journal replay path.
The journal remains authoritative and its incomplete-tail truncation behavior
is unchanged.

Host-validator closures are not serializable. Checkpoint creation is therefore
rejected while validators are registered, and checkpoint loading is disabled
when an open supplies validators.

## Measured boundary

The ramped library report measures checkpoint creation, checkpoint size, full
journal replay to query-ready state and checkpoint recovery to query-ready
state from the same database. It also asserts exact world, version, active-slot,
record and semantic-query parity for both paths.

This format intentionally stores semantic frames rather than raw Rust memory.
It is relocatable and independently validated, but restoring the VM still
executes the already-accepted operations once. Persisting raw POD arena
segments would be a separate format and portability experiment.
