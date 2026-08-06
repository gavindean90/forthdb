include!("lib.rs");

mod admission_epoch;
mod file_epoch_controller;
mod file_epoch_store;
mod file_store;
mod history_lifecycle;
mod io_uring_epoch_io;
mod mmap_store;
mod mmap_vm_snapshot;
mod queued;
mod queued_controller;
pub mod semantic_isa;
pub mod stack_vm;
pub mod transaction_ast;
mod writer_lock;

pub use admission_epoch::{
    AdmissionEpochBatchSubmitError, AdmissionEpochController, AdmissionEpochMetrics,
    AdmissionEpochOpenError, AdmissionEpochReceipt, AdmissionEpochSubmitError,
    AdmissionEpochTicket, AdmissionEpochTicketOutcome,
};
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
pub use io_uring_epoch_io::{IoUringEpochFileIo, IoUringEpochStore};
pub use mmap_store::{MmapCommitStore, MmapCommitStoreError};
pub use mmap_vm_snapshot::MmapSnapshotMetadata;
pub use queued::{
    AcceptedIntent, EpochOutcome, EpochPlan, IntentAtom, IntentFact, IntentPrecondition,
    IntentRejection, QueuedIntent, RejectedIntent, TempEntity, derive_epoch, derive_epoch_world,
};
pub use queued_controller::{
    CommitTicket, ControllerConfigError, ControllerStopped, QueuedControllerMetrics,
    QueuedIntentController, SubmitError, TicketOutcome, TicketPhase, TicketRejection, TicketState,
    TicketWaitError, BatchPolicy,
};

pub use writer_lock::{WriterLease, WriterLeaseError, lock_path as writer_lock_path};
