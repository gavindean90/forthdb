include!("lib.rs");

mod file_epoch_controller;
mod file_epoch_store;
mod file_store;
mod history_lifecycle;
mod io_uring_epoch_io;
mod io_uring_store;
mod mmap_store;
mod queued;
mod queued_controller;
mod writer_lock;

pub use file_epoch_controller::{
    DurableCommitTicket, DurableControllerConfigError, DurableControllerOpenError,
    DurableControllerState, DurableControllerStopped, DurableQueuedControllerMetrics,
    DurableQueuedIntentController, DurableShutdownReport, DurableSubmitError, DurableTicketOutcome,
    DurableTicketPhase, DurableTicketRejection, DurableTicketState, DurableTicketStopReason,
    DurableTicketWaitError,
};
pub use file_epoch_store::{
    EpochFileIo, EpochIoPhase, EpochPersistMetrics, FileEpochMetrics, FileEpochState,
    FileEpochStore, FileEpochStoreError, FileEpochSyncPolicy, StdEpochFileIo,
};
pub use file_store::{FileCommitStore, FileCommitStoreError};
pub use io_uring_epoch_io::{IoUringEpochFileIo, IoUringEpochStore, IoUringEpochStrategy};
pub use io_uring_store::{IoUringCommitStore, IoUringCommitStoreError};
pub use mmap_store::{MmapCommitStore, MmapCommitStoreError};
pub use queued::{
    AcceptedIntent, EpochOutcome, EpochPlan, IntentAtom, IntentFact, IntentPrecondition,
    IntentRejection, QueuedIntent, RejectedIntent, TempEntity, derive_epoch,
};
pub use queued_controller::{
    CommitTicket, ControllerConfigError, ControllerStopped, QueuedControllerMetrics,
    QueuedIntentController, SubmitError, TicketOutcome, TicketPhase, TicketRejection, TicketState,
    TicketWaitError,
};

pub use writer_lock::{WriterLease, WriterLeaseError, lock_path as writer_lock_path};
