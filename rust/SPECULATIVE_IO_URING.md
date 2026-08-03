# One-Epoch-Ahead io_uring Experiment

## Question

Can ForthDB use io_uring to separate semantic preparation from physical
durability, even though io_uring did not beat ordinary `write` plus
`fdatasync` as a synchronous transport?

The ordinary per-epoch controller remains the default and benchmark control.
This experiment is opt-in through
`DurableQueuedIntentController::new_speculative` or
`open_owned_speculative` on Linux.

## Pipeline

The controller retains one committer and one ordered physical log:

```text
derive epoch N
submit contiguous WRITE(N) -> FSYNC(DATASYNC)
    while N is in flight: derive at most epoch N+1
reap and validate every CQE for N
finalize physical history for N
publish N and resolve its tickets
submit N+1
```

There is no dedicated blocking writer and no second durability epoch in
flight. ForthDB owns epoch construction, predecessor order, publication,
ticket truth and failure handling. The kernel owns progress of the submitted
write and synchronization while ForthDB prepares the private successor.

## Ownership

The submitted arena remains owned by `PendingIoUringEpoch` until both the write
and synchronization CQEs have been reaped. The file store and database commit
lock remain exclusively held across submission, speculative derivation and
completion. Strict transactions therefore cannot interleave with a provisional
chain.

Epoch N+1 is derived from N's private tail but no N+1 bytes are submitted and
no N+1 result is published before N completes successfully.

## Failure

If N fails, the established Milestone 6B repair or poisoning path runs before
the submitted arena is released. N publishes nothing. A prepared N+1 is
rederived from the still-published head after successful repair because its
original predecessor never became durable.

Validator panics are caught during private derivation. Ticket and controller
lifecycle behavior remains governed by the Milestone 6D worker-failure and
shutdown contracts.

## Measurement

The existing io_uring epoch benchmark adds
`io_uring_speculative_one_ahead` beside the unchanged ordinary per-epoch and
synchronous ring arms. It records how many successors were prepared and how
many required rederivation.

This experiment succeeds only if it preserves canonical recovery and ticket
ordering. A throughput win is workload-dependent rather than a correctness
requirement. The decisive comparison remains the ordinary per-epoch control.

