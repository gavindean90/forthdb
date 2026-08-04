# Durable Token VM Integration

## Contract

`AdmissionEpochController::open_vm` runs the durable admission journal through
the framed token VM. The journal remains authoritative and retains its version
1 canonical `QueuedIntent` encoding. Live execution and recovery both compile
those durable intents deterministically into the same VM instruction stream.

The complete path is:

```text
QueuedIntent epoch
    -> canonical checksummed journal record
    -> io_uring WRITE + DATASYNC
    -> deterministic token compilation
    -> framed VM trial execution and rollback
    -> one immutable VM-backed World root
    -> ticket outcomes and reader publication
```

The VM owns ordered trial state, temporary allocation, slot visibility,
`FORGET` unmasking, and predecessor-relative precondition evaluation. Rejected
intents change neither its arena frontiers nor its allocator. Accepted
operations are accumulated into the canonical epoch frame without immediately
executing them a second time in the older `ForthDb` kernel. Existing query,
history, identity, and application APIs remain available through a lazy,
cached native query projection.

## Recovery

Recovery starts from genesis, rebuilds the token dictionaries in first-seen
journal order, executes every sound admission epoch through the VM, and
reconstructs the same world identity, version, frame history, allocator, and VM
root as live execution. The native query projection is constructed only
if a recovered reader asks for it. An incomplete journal tail is still
truncated before replay.

No second outcome log or VM-specific durable format is required. Changing the
compiler or VM semantics is therefore a compatibility-sensitive change because
recovery must continue to reproduce historical worlds.

## Validators

General host-closure validators require the rich `CandidateWorld` API. Epochs
with registered validators use the established world materializer, then apply
the accepted canonical frame to the VM so later validator-free epochs can
resume on the fast path. Metrics report VM and world-materializer epoch counts
separately; the library workloads assert that they remain entirely on the VM.

## Reader model

Published application readers still receive `Arc<World>` snapshots. For VM
epochs the snapshot is an immutable semantic root over the canonical frame and
VM frontiers. Identity, counts, tickets and history do not require the legacy
kernel. The first call to `resolve`, `definitions`, `display_name` or `query`
builds record-ordered indexes directly from immutable history and caches them
on the queried root. See [`VM_WORLD_ROOTS.md`](VM_WORLD_ROOTS.md) and
[`VM_DIRECT_QUERIES.md`](VM_DIRECT_QUERIES.md).

The query projection is still rebuilt after process recovery. Persisting a
compact projection checkpoint is the next recovery-specific experiment, not
part of this query milestone.

## Evidence

The integration is gated by:

- multi-epoch differential parity against `derive_epoch_world`
- dependent slot preconditions within an epoch
- rejection and allocator rollback
- `FORGET` and prior-definition visibility
- validator fallback followed by VM resumption
- durable io_uring publication and VM-based reopen
- the complete library application and exact recovery
- the ramped library workload against both VM and previous materializers

Set `FORTHDB_LIBRARY_MATERIALIZER=world` to retain the previous materializer as
a hosted comparison. The library applications default to `vm`.
