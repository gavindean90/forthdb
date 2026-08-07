use super::queued_controller::{AdaptiveState, BatchSealReason};
use super::*;
use crate::queued::VmEpochMaterializer;
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PHASE_QUEUED: u8 = 0;
const PHASE_CLAIMED: u8 = 1;
const PHASE_RESOLVED: u8 = 2;

const CONTROLLER_STARTING: u8 = 0;
const CONTROLLER_RUNNING: u8 = 1;
const CONTROLLER_DRAINING: u8 = 2;
const CONTROLLER_CLOSED: u8 = 3;
const CONTROLLER_POISONED: u8 = 4;

static NEXT_DURABLE_TICKET_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableControllerState {
    Starting,
    Running,
    Draining,
    Closed,
    Poisoned,
}

impl DurableControllerState {
    fn from_raw(value: u8) -> Self {
        match value {
            CONTROLLER_STARTING => Self::Starting,
            CONTROLLER_RUNNING => Self::Running,
            CONTROLLER_DRAINING => Self::Draining,
            CONTROLLER_CLOSED => Self::Closed,
            CONTROLLER_POISONED => Self::Poisoned,
            _ => unreachable!("invalid durable controller state"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableTicketPhase {
    Queued,
    Claimed,
    Resolved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurableTicketState {
    pub phase: DurableTicketPhase,
    pub abandoned: bool,
}

struct DurableTicketLifecycle {
    phase: AtomicU8,
    abandoned: AtomicBool,
}

impl DurableTicketLifecycle {
    fn new() -> Self {
        Self {
            phase: AtomicU8::new(PHASE_QUEUED),
            abandoned: AtomicBool::new(false),
        }
    }

    fn claim(&self) {
        self.phase.store(PHASE_CLAIMED, Ordering::Release);
    }

    fn resolve(&self) {
        self.phase.store(PHASE_RESOLVED, Ordering::Release);
    }

    fn abandon(&self) -> bool {
        !self.abandoned.swap(true, Ordering::AcqRel)
    }

    fn snapshot(&self) -> DurableTicketState {
        let phase = match self.phase.load(Ordering::Acquire) {
            PHASE_QUEUED => DurableTicketPhase::Queued,
            PHASE_CLAIMED => DurableTicketPhase::Claimed,
            PHASE_RESOLVED => DurableTicketPhase::Resolved,
            _ => unreachable!("invalid durable ticket phase"),
        };
        DurableTicketState {
            phase,
            abandoned: self.abandoned.load(Ordering::Acquire),
        }
    }
}

#[derive(Debug)]
pub enum DurableTicketRejection {
    WorldPrecondition {
        expected: WorldId,
        actual: WorldId,
    },
    SlotPrecondition {
        slot: SlotId,
        expected: Option<Fact>,
        actual: Option<Fact>,
    },
    UnknownTemporaryEntity(TempEntity),
    Candidate(String),
    Validation(String),
}

impl DurableTicketRejection {
    pub(crate) fn from_intent(error: &IntentRejection) -> Self {
        match error {
            IntentRejection::WorldPrecondition { expected, actual } => Self::WorldPrecondition {
                expected: *expected,
                actual: *actual,
            },
            IntentRejection::SlotPrecondition {
                slot,
                expected,
                actual,
            } => Self::SlotPrecondition {
                slot: slot.clone(),
                expected: expected.clone(),
                actual: actual.clone(),
            },
            IntentRejection::UnknownTemporaryEntity(entity) => {
                Self::UnknownTemporaryEntity(*entity)
            }
            IntentRejection::Candidate(error) => Self::Candidate(error.to_string()),
            IntentRejection::Validation(message) => Self::Validation(message.clone()),
        }
    }
}

impl fmt::Display for DurableTicketRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorldPrecondition { expected, actual } => write!(
                formatter,
                "queued intent expected predecessor {expected}, found {actual}"
            ),
            Self::SlotPrecondition {
                slot,
                expected,
                actual,
            } => write!(
                formatter,
                "queued intent slot precondition failed for {slot:?}: expected {expected:?}, found {actual:?}"
            ),
            Self::UnknownTemporaryEntity(entity) => write!(
                formatter,
                "queued intent referenced temporary entity {} from another scope or before allocation",
                entity.index()
            ),
            Self::Candidate(message) => write!(formatter, "queued candidate failed: {message}"),
            Self::Validation(message) => {
                write!(formatter, "queued candidate validation failed: {message}")
            }
        }
    }
}

impl Error for DurableTicketRejection {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableTicketStopReason {
    ShutdownBeforeClaim,
    WorkerFailed(String),
}

#[derive(Debug)]
pub enum DurableTicketOutcome {
    Accepted {
        world: Arc<World>,
        frame: Arc<CommitFrame>,
        entities: BTreeMap<TempEntity, EntityId>,
    },
    Rejected(DurableTicketRejection),
    DurabilityFailed(String),
    Stopped(DurableTicketStopReason),
}

impl DurableTicketOutcome {
    pub fn world(&self) -> Option<Arc<World>> {
        match self {
            Self::Accepted { world, .. } => Some(world.clone()),
            Self::Rejected(_) | Self::DurabilityFailed(_) | Self::Stopped(_) => None,
        }
    }

    pub fn rejection(&self) -> Option<&DurableTicketRejection> {
        match self {
            Self::Rejected(error) => Some(error),
            Self::Accepted { .. } | Self::DurabilityFailed(_) | Self::Stopped(_) => None,
        }
    }

    pub fn durability_error(&self) -> Option<&str> {
        match self {
            Self::DurabilityFailed(message) => Some(message),
            Self::Accepted { .. } | Self::Rejected(_) | Self::Stopped(_) => None,
        }
    }

    pub fn stop_reason(&self) -> Option<&DurableTicketStopReason> {
        match self {
            Self::Stopped(reason) => Some(reason),
            Self::Accepted { .. } | Self::Rejected(_) | Self::DurabilityFailed(_) => None,
        }
    }
}

#[derive(Debug)]
pub struct DurableTicketWaitError;

impl fmt::Display for DurableTicketWaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "durable queued-intent controller stopped before resolving ticket"
        )
    }
}

impl Error for DurableTicketWaitError {}

pub struct DurableCommitTicket {
    id: u64,
    receiver: Option<Receiver<DurableTicketOutcome>>,
    lifecycle: Arc<DurableTicketLifecycle>,
    metrics: Arc<DurableControllerMetricsInner>,
    observed: bool,
}

impl fmt::Debug for DurableCommitTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableCommitTicket")
            .field("id", &self.id)
            .field("state", &self.state())
            .finish()
    }
}

impl DurableCommitTicket {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn state(&self) -> DurableTicketState {
        self.lifecycle.snapshot()
    }

    pub fn wait(mut self) -> Result<DurableTicketOutcome, DurableTicketWaitError> {
        let receiver = self
            .receiver
            .take()
            .expect("durable ticket receiver can be consumed only once");
        let result = receiver.recv().map_err(|_| DurableTicketWaitError);
        self.observed = true;
        result
    }

    pub fn try_wait(&mut self) -> Result<Option<DurableTicketOutcome>, DurableTicketWaitError> {
        let receiver = self
            .receiver
            .as_ref()
            .expect("durable ticket receiver can be consumed only once");
        match receiver.try_recv() {
            Ok(outcome) => {
                self.observed = true;
                self.receiver.take();
                Ok(Some(outcome))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                self.observed = true;
                self.receiver.take();
                Err(DurableTicketWaitError)
            }
        }
    }
}

impl Drop for DurableCommitTicket {
    fn drop(&mut self) {
        if !self.observed && self.lifecycle.abandon() {
            self.metrics
                .abandoned_tickets
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Debug)]
pub enum DurableSubmitError {
    Full(QueuedIntent),
    Closed(QueuedIntent),
    Poisoned {
        intent: QueuedIntent,
        reason: String,
    },
}

impl DurableSubmitError {
    pub fn into_intent(self) -> QueuedIntent {
        match self {
            Self::Full(intent) | Self::Closed(intent) | Self::Poisoned { intent, .. } => intent,
        }
    }
}

impl fmt::Display for DurableSubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => write!(formatter, "durable queued-intent ingress is full"),
            Self::Closed(_) => write!(formatter, "durable queued-intent controller is not running"),
            Self::Poisoned { reason, .. } => {
                write!(
                    formatter,
                    "durable queued-intent controller is poisoned: {reason}"
                )
            }
        }
    }
}

impl Error for DurableSubmitError {}

#[derive(Debug)]
pub enum DurableControllerConfigError {
    ZeroCapacity,
    ZeroBatchSize,
    Spawn(std::io::Error),
}

impl fmt::Display for DurableControllerConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCapacity => write!(formatter, "durable ingress capacity must be nonzero"),
            Self::ZeroBatchSize => write!(formatter, "durable maximum batch size must be nonzero"),
            Self::Spawn(error) => write!(formatter, "failed to start durable committer: {error}"),
        }
    }
}

impl Error for DurableControllerConfigError {}

#[derive(Debug)]
pub enum DurableControllerOpenError {
    WriterLease(WriterLeaseError),
    Store(FileEpochStoreError),
    History(CandidateError),
    Controller(DurableControllerConfigError),
}

impl fmt::Display for DurableControllerOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WriterLease(error) => write!(formatter, "writer ownership failed: {error}"),
            Self::Store(error) => write!(formatter, "durable store open failed: {error}"),
            Self::History(error) => {
                write!(formatter, "durable history reconstruction failed: {error}")
            }
            Self::Controller(error) => {
                write!(formatter, "durable controller start failed: {error}")
            }
        }
    }
}

impl Error for DurableControllerOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::WriterLease(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::History(error) => Some(error),
            Self::Controller(error) => Some(error),
        }
    }
}

impl From<WriterLeaseError> for DurableControllerOpenError {
    fn from(value: WriterLeaseError) -> Self {
        Self::WriterLease(value)
    }
}

impl From<FileEpochStoreError> for DurableControllerOpenError {
    fn from(value: FileEpochStoreError) -> Self {
        Self::Store(value)
    }
}

impl From<CandidateError> for DurableControllerOpenError {
    fn from(value: CandidateError) -> Self {
        Self::History(value)
    }
}

impl From<DurableControllerConfigError> for DurableControllerOpenError {
    fn from(value: DurableControllerConfigError) -> Self {
        Self::Controller(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableControllerStopped {
    pub state: DurableControllerState,
    pub reason: Option<String>,
}

impl fmt::Display for DurableControllerStopped {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(reason) = &self.reason {
            write!(
                formatter,
                "durable controller stopped in {:?}: {reason}",
                self.state
            )
        } else {
            write!(formatter, "durable controller stopped in {:?}", self.state)
        }
    }
}

impl Error for DurableControllerStopped {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableShutdownReport {
    pub previous_state: DurableControllerState,
    pub final_state: DurableControllerState,
    pub queued_stopped: u64,
    pub worker_failures: u64,
    pub reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurableQueuedControllerMetrics {
    pub capacity: usize,
    pub max_batch: usize,
    pub state: DurableControllerState,
    pub submitted: u64,
    pub backpressured: u64,
    pub claimed: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub durability_failed: u64,
    pub shutdown_before_claim: u64,
    pub worker_failed: u64,
    pub epochs: u64,
    pub speculative_epochs_prepared: u64,
    pub speculative_epochs_rederived: u64,
    pub abandoned_tickets: u64,
    pub completion_delivery_failures: u64,
    pub queue_depth: u64,
    pub maximum_queue_depth: u64,
    pub in_flight: u64,
    pub worker_alive: bool,
    pub queue_wait_nanos: u64,
    pub derive_nanos: u64,
    pub persist_nanos: u64,
    pub publish_nanos: u64,
    pub delivery_nanos: u64,
    pub epoch_total_nanos: u64,
    pub batches_sealed_by_capacity: u64,
    pub batches_sealed_by_timeout: u64,
    pub batches_sealed_by_drain: u64,
    pub batches_sealed_by_width: u64,
    pub batches_sealed_by_latency: u64,
    pub batches_sealed_by_low_traffic: u64,
    pub batches_sealed_by_source_stalled: u64,
    pub batches_sealed_by_barrier: u64,
    pub maximum_target_width: usize,
    pub total_adaptive_probe_wait_ns: u64,
    pub maximum_oldest_age_at_seal_ns: u64,
}

impl DurableQueuedControllerMetrics {
    pub fn estimated_pipeline_speedup_ceiling(&self) -> Option<f64> {
        if self.derive_nanos == 0 || self.persist_nanos == 0 {
            return None;
        }
        let serial = self.derive_nanos.saturating_add(self.persist_nanos) as f64;
        let overlapped = self.derive_nanos.max(self.persist_nanos) as f64;
        Some(serial / overlapped)
    }
}

#[derive(Default)]
struct DurableControllerMetricsInner {
    state: AtomicU8,
    poison_reason: Mutex<Option<String>>,
    submitted: AtomicU64,
    backpressured: AtomicU64,
    claimed: AtomicU64,
    accepted: AtomicU64,
    rejected: AtomicU64,
    durability_failed: AtomicU64,
    shutdown_before_claim: AtomicU64,
    worker_failed: AtomicU64,
    epochs: AtomicU64,
    speculative_epochs_prepared: AtomicU64,
    speculative_epochs_rederived: AtomicU64,
    abandoned_tickets: AtomicU64,
    completion_delivery_failures: AtomicU64,
    queue_depth: AtomicU64,
    maximum_queue_depth: AtomicU64,
    in_flight: AtomicU64,
    worker_alive: AtomicBool,
    queue_wait_nanos: AtomicU64,
    derive_nanos: AtomicU64,
    persist_nanos: AtomicU64,
    publish_nanos: AtomicU64,
    delivery_nanos: AtomicU64,
    epoch_total_nanos: AtomicU64,
    batches_sealed_by_capacity: AtomicU64,
    batches_sealed_by_timeout: AtomicU64,
    batches_sealed_by_drain: AtomicU64,
    batches_sealed_by_width: AtomicU64,
    batches_sealed_by_latency: AtomicU64,
    batches_sealed_by_low_traffic: AtomicU64,
    batches_sealed_by_source_stalled: AtomicU64,
    batches_sealed_by_barrier: AtomicU64,
    maximum_target_width: AtomicU64,
    total_adaptive_probe_wait_ns: AtomicU64,
    maximum_oldest_age_at_seal_ns: AtomicU64,
}

impl DurableControllerMetricsInner {
    fn state(&self) -> DurableControllerState {
        DurableControllerState::from_raw(self.state.load(Ordering::Acquire))
    }

    fn set_state(&self, state: DurableControllerState) {
        let raw = match state {
            DurableControllerState::Starting => CONTROLLER_STARTING,
            DurableControllerState::Running => CONTROLLER_RUNNING,
            DurableControllerState::Draining => CONTROLLER_DRAINING,
            DurableControllerState::Closed => CONTROLLER_CLOSED,
            DurableControllerState::Poisoned => CONTROLLER_POISONED,
        };
        self.state.store(raw, Ordering::Release);
    }

    fn poison(&self, reason: impl Into<String>) -> String {
        let reason = reason.into();
        *self
            .poison_reason
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason.clone());
        self.set_state(DurableControllerState::Poisoned);
        reason
    }

    fn reason(&self) -> Option<String> {
        self.poison_reason
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn stopped(&self) -> DurableControllerStopped {
        DurableControllerStopped {
            state: self.state(),
            reason: self.reason(),
        }
    }

    fn reserve_ingress(&self, capacity: usize) -> Option<u64> {
        let capacity = capacity as u64;
        let mut current = self.queue_depth.load(Ordering::Acquire);
        loop {
            if current >= capacity {
                return None;
            }
            match self.queue_depth.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(current + 1),
                Err(actual) => current = actual,
            }
        }
    }

    fn release_ingress(&self) {
        let previous = self.queue_depth.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "durable ingress reservation underflow");
    }

    fn observe_queue_depth(&self, depth: u64) {
        let mut previous = self.maximum_queue_depth.load(Ordering::Relaxed);
        while depth > previous {
            match self.maximum_queue_depth.compare_exchange_weak(
                previous,
                depth,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => previous = actual,
            }
        }
    }

    fn record_seal_reason(&self, reason: BatchSealReason) {
        match reason {
            BatchSealReason::Capacity => {
                self.batches_sealed_by_capacity
                    .fetch_add(1, Ordering::Relaxed);
            }
            BatchSealReason::Timeout => {
                self.batches_sealed_by_timeout
                    .fetch_add(1, Ordering::Relaxed);
            }
            BatchSealReason::Drain => {
                self.batches_sealed_by_drain.fetch_add(1, Ordering::Relaxed);
            }
            BatchSealReason::Width => {
                self.batches_sealed_by_width.fetch_add(1, Ordering::Relaxed);
            }
            BatchSealReason::Latency => {
                self.batches_sealed_by_latency
                    .fetch_add(1, Ordering::Relaxed);
            }
            BatchSealReason::LowTraffic => {
                self.batches_sealed_by_low_traffic
                    .fetch_add(1, Ordering::Relaxed);
            }
            BatchSealReason::SourceStalled => {
                self.batches_sealed_by_source_stalled
                    .fetch_add(1, Ordering::Relaxed);
            }
            BatchSealReason::Barrier => {
                self.batches_sealed_by_barrier
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn record_max_target_width(&self, width: usize) {
        let width = width as u64;
        let mut prev = self.maximum_target_width.load(Ordering::Relaxed);
        while width > prev {
            match self.maximum_target_width.compare_exchange_weak(
                prev,
                width,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => prev = actual,
            }
        }
    }

    fn record_oldest_age_at_seal(&self, age: std::time::Duration) {
        let age_ns = age.as_nanos() as u64;
        let mut prev = self.maximum_oldest_age_at_seal_ns.load(Ordering::Relaxed);
        while age_ns > prev {
            match self.maximum_oldest_age_at_seal_ns.compare_exchange_weak(
                prev,
                age_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => prev = actual,
            }
        }
    }

    fn snapshot(&self, capacity: usize, max_batch: usize) -> DurableQueuedControllerMetrics {
        DurableQueuedControllerMetrics {
            capacity,
            max_batch,
            state: self.state(),
            submitted: self.submitted.load(Ordering::Relaxed),
            backpressured: self.backpressured.load(Ordering::Relaxed),
            claimed: self.claimed.load(Ordering::Relaxed),
            accepted: self.accepted.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            durability_failed: self.durability_failed.load(Ordering::Relaxed),
            shutdown_before_claim: self.shutdown_before_claim.load(Ordering::Relaxed),
            worker_failed: self.worker_failed.load(Ordering::Relaxed),
            epochs: self.epochs.load(Ordering::Relaxed),
            speculative_epochs_prepared: self.speculative_epochs_prepared.load(Ordering::Relaxed),
            speculative_epochs_rederived: self.speculative_epochs_rederived.load(Ordering::Relaxed),
            abandoned_tickets: self.abandoned_tickets.load(Ordering::Relaxed),
            completion_delivery_failures: self.completion_delivery_failures.load(Ordering::Relaxed),
            queue_depth: self.queue_depth.load(Ordering::Acquire),
            maximum_queue_depth: self.maximum_queue_depth.load(Ordering::Relaxed),
            in_flight: self.in_flight.load(Ordering::Acquire),
            worker_alive: self.worker_alive.load(Ordering::Acquire),
            queue_wait_nanos: self.queue_wait_nanos.load(Ordering::Relaxed),
            derive_nanos: self.derive_nanos.load(Ordering::Relaxed),
            persist_nanos: self.persist_nanos.load(Ordering::Relaxed),
            publish_nanos: self.publish_nanos.load(Ordering::Relaxed),
            delivery_nanos: self.delivery_nanos.load(Ordering::Relaxed),
            epoch_total_nanos: self.epoch_total_nanos.load(Ordering::Relaxed),
            batches_sealed_by_capacity: self.batches_sealed_by_capacity.load(Ordering::Relaxed),
            batches_sealed_by_timeout: self.batches_sealed_by_timeout.load(Ordering::Relaxed),
            batches_sealed_by_drain: self.batches_sealed_by_drain.load(Ordering::Relaxed),
            batches_sealed_by_width: self.batches_sealed_by_width.load(Ordering::Relaxed),
            batches_sealed_by_latency: self.batches_sealed_by_latency.load(Ordering::Relaxed),
            batches_sealed_by_low_traffic: self
                .batches_sealed_by_low_traffic
                .load(Ordering::Relaxed),
            batches_sealed_by_source_stalled: self
                .batches_sealed_by_source_stalled
                .load(Ordering::Relaxed),
            batches_sealed_by_barrier: self.batches_sealed_by_barrier.load(Ordering::Relaxed),
            maximum_target_width: self.maximum_target_width.load(Ordering::Relaxed) as usize,
            total_adaptive_probe_wait_ns: self.total_adaptive_probe_wait_ns.load(Ordering::Relaxed),
            maximum_oldest_age_at_seal_ns: self
                .maximum_oldest_age_at_seal_ns
                .load(Ordering::Relaxed),
        }
    }
}

struct DurableStagedIntent {
    intent: QueuedIntent,
    completion: mpsc::Sender<DurableTicketOutcome>,
    lifecycle: Arc<DurableTicketLifecycle>,
    admitted_at: Instant,
}

enum DurableControllerCommand {
    Intent(DurableStagedIntent),
    Barrier(mpsc::Sender<Result<(), DurableControllerStopped>>),
}

pub struct DurableQueuedIntentController<I: EpochFileIo = StdEpochFileIo> {
    database: Arc<Database<FileEpochStore<I>>>,
    sender: Arc<Mutex<Option<SyncSender<DurableControllerCommand>>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    writer_lease: Mutex<Option<WriterLease>>,
    shutdown_lock: Mutex<()>,
    metrics: Arc<DurableControllerMetricsInner>,
    capacity: usize,
    max_batch: usize,
}

impl<I: EpochFileIo + 'static> fmt::Debug for DurableQueuedIntentController<I> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableQueuedIntentController")
            .field("capacity", &self.capacity)
            .field("max_batch", &self.max_batch)
            .field("metrics", &self.metrics())
            .finish()
    }
}

impl DurableQueuedIntentController<StdEpochFileIo> {
    pub fn open_owned(
        path: impl AsRef<Path>,
        sync_policy: FileEpochSyncPolicy,
        capacity: usize,
        max_batch: usize,
    ) -> Result<Self, DurableControllerOpenError> {
        Self::open_owned_with_policy(
            path,
            sync_policy,
            capacity,
            BatchPolicy::ImmediateDrain { max_batch },
        )
    }

    pub fn open_owned_with_policy(
        path: impl AsRef<Path>,
        sync_policy: FileEpochSyncPolicy,
        capacity: usize,
        batch_policy: BatchPolicy,
    ) -> Result<Self, DurableControllerOpenError> {
        let path = path.as_ref();
        let lease = WriterLease::acquire(path)?;
        let store = FileEpochStore::open(path, sync_policy)?;
        let database = Arc::new(Database::new(store)?);
        Self::new_with_lease_policy(database, capacity, batch_policy, Some(lease))
            .map_err(Into::into)
    }
}

#[cfg(target_os = "linux")]
impl DurableQueuedIntentController<IoUringEpochFileIo> {
    /// Starts the one-epoch-ahead io_uring experiment.
    ///
    /// The ordinary per-epoch controller remains the default. This opt-in
    /// controller submits one contiguous WRITE + DATASYNC epoch, derives at
    /// most one successor while durability is in flight, and publishes in
    /// durable order.
    pub fn new_speculative(
        database: Arc<Database<FileEpochStore<IoUringEpochFileIo>>>,
        capacity: usize,
        max_batch: usize,
    ) -> Result<Self, DurableControllerConfigError> {
        Self::new_with_runner(
            database,
            capacity,
            BatchPolicy::ImmediateDrain { max_batch },
            None,
            run_speculative_io_uring_worker,
        )
    }

    pub fn open_owned_speculative(
        path: impl AsRef<Path>,
        capacity: usize,
        max_batch: usize,
        ring_entries: u32,
    ) -> Result<Self, DurableControllerOpenError> {
        let path = path.as_ref();
        let lease = WriterLease::acquire(path)?;
        let store = IoUringEpochFileIo::open_store_with_entries(path, ring_entries)?;
        let database = Arc::new(Database::new(store)?);
        Self::new_with_runner(
            database,
            capacity,
            BatchPolicy::ImmediateDrain { max_batch },
            Some(lease),
            run_speculative_io_uring_worker,
        )
        .map_err(Into::into)
    }
}

impl<I: EpochFileIo + 'static> DurableQueuedIntentController<I> {
    /// Starts a controller without acquiring a process writer lease.
    ///
    /// This remains useful for injected-I/O tests and callers that manage an
    /// equivalent external lease. Production writable opens should prefer
    /// `open_owned`.
    pub fn new(
        database: Arc<Database<FileEpochStore<I>>>,
        capacity: usize,
        max_batch: usize,
    ) -> Result<Self, DurableControllerConfigError> {
        Self::new_with_policy(
            database,
            capacity,
            BatchPolicy::ImmediateDrain { max_batch },
        )
    }

    pub fn new_with_policy(
        database: Arc<Database<FileEpochStore<I>>>,
        capacity: usize,
        policy: BatchPolicy,
    ) -> Result<Self, DurableControllerConfigError> {
        Self::new_with_lease_policy(database, capacity, policy, None)
    }

    fn new_with_lease_policy(
        database: Arc<Database<FileEpochStore<I>>>,
        capacity: usize,
        policy: BatchPolicy,
        lease: Option<WriterLease>,
    ) -> Result<Self, DurableControllerConfigError> {
        Self::new_with_runner(database, capacity, policy, lease, run_durable_worker::<I>)
    }

    fn new_with_runner<F>(
        database: Arc<Database<FileEpochStore<I>>>,
        capacity: usize,
        policy: BatchPolicy,
        lease: Option<WriterLease>,
        runner: F,
    ) -> Result<Self, DurableControllerConfigError>
    where
        F: FnOnce(
                Arc<Database<FileEpochStore<I>>>,
                Receiver<DurableControllerCommand>,
                BatchPolicy,
                Arc<DurableControllerMetricsInner>,
                Arc<Mutex<Option<SyncSender<DurableControllerCommand>>>>,
            ) + Send
            + 'static,
    {
        let max_batch = match policy {
            BatchPolicy::ImmediateDrain { max_batch }
            | BatchPolicy::Coalesce { max_batch, .. }
            | BatchPolicy::Adaptive { max_batch, .. } => max_batch,
        };
        if capacity == 0 {
            return Err(DurableControllerConfigError::ZeroCapacity);
        }
        if max_batch == 0 {
            return Err(DurableControllerConfigError::ZeroBatchSize);
        }
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let sender = Arc::new(Mutex::new(Some(sender)));
        let metrics = Arc::new(DurableControllerMetricsInner::default());
        metrics.set_state(DurableControllerState::Starting);
        metrics.worker_alive.store(true, Ordering::Release);
        let worker_database = database.clone();
        let worker_metrics = metrics.clone();
        let worker_sender = sender.clone();
        let worker = thread::Builder::new()
            .name("forthdb-durable-committer".to_owned())
            .spawn(move || {
                runner(
                    worker_database,
                    receiver,
                    policy,
                    worker_metrics,
                    worker_sender,
                )
            })
            .map_err(|error| {
                metrics.worker_alive.store(false, Ordering::Release);
                metrics.poison(format!("failed to spawn durable committer: {error}"));
                DurableControllerConfigError::Spawn(error)
            })?;
        metrics.set_state(DurableControllerState::Running);
        Ok(Self {
            database,
            sender,
            worker: Mutex::new(Some(worker)),
            writer_lease: Mutex::new(lease),
            shutdown_lock: Mutex::new(()),
            metrics,
            capacity,
            max_batch,
        })
    }

    pub fn database(&self) -> Arc<Database<FileEpochStore<I>>> {
        self.database.clone()
    }

    pub fn state(&self) -> DurableControllerState {
        self.metrics.state()
    }

    pub fn poison_reason(&self) -> Option<String> {
        self.metrics.reason()
    }

    pub fn submit(&self, intent: QueuedIntent) -> Result<DurableCommitTicket, DurableSubmitError> {
        let sender_guard = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match self.metrics.state() {
            DurableControllerState::Poisoned => {
                return Err(DurableSubmitError::Poisoned {
                    intent,
                    reason: self
                        .metrics
                        .reason()
                        .unwrap_or_else(|| "worker or store state is uncertain".to_owned()),
                });
            }
            DurableControllerState::Running => {}
            DurableControllerState::Starting
            | DurableControllerState::Draining
            | DurableControllerState::Closed => return Err(DurableSubmitError::Closed(intent)),
        }
        let Some(sender) = sender_guard.as_ref() else {
            return Err(DurableSubmitError::Closed(intent));
        };
        let Some(depth) = self.metrics.reserve_ingress(self.capacity) else {
            self.metrics.backpressured.fetch_add(1, Ordering::Relaxed);
            return Err(DurableSubmitError::Full(intent));
        };
        let lifecycle = Arc::new(DurableTicketLifecycle::new());
        let (completion, receiver) = mpsc::channel();
        let command = DurableControllerCommand::Intent(DurableStagedIntent {
            intent,
            completion,
            lifecycle: lifecycle.clone(),
            admitted_at: Instant::now(),
        });
        match sender.try_send(command) {
            Ok(()) => {
                self.metrics.observe_queue_depth(depth);
                self.metrics.submitted.fetch_add(1, Ordering::Relaxed);
                Ok(DurableCommitTicket {
                    id: NEXT_DURABLE_TICKET_ID.fetch_add(1, Ordering::Relaxed),
                    receiver: Some(receiver),
                    lifecycle,
                    metrics: self.metrics.clone(),
                    observed: false,
                })
            }
            Err(TrySendError::Full(DurableControllerCommand::Intent(staged))) => {
                self.metrics.release_ingress();
                self.metrics.backpressured.fetch_add(1, Ordering::Relaxed);
                Err(DurableSubmitError::Full(staged.intent))
            }
            Err(TrySendError::Disconnected(DurableControllerCommand::Intent(staged))) => {
                self.metrics.release_ingress();
                let reason = self
                    .metrics
                    .poison("durable worker command channel disconnected unexpectedly");
                Err(DurableSubmitError::Poisoned {
                    intent: staged.intent,
                    reason,
                })
            }
            Err(TrySendError::Full(DurableControllerCommand::Barrier(_)))
            | Err(TrySendError::Disconnected(DurableControllerCommand::Barrier(_))) => {
                unreachable!()
            }
        }
    }

    pub fn flush(&self) -> Result<(), DurableControllerStopped> {
        let sender = {
            let guard = self
                .sender
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.metrics.state() != DurableControllerState::Running {
                return Err(self.metrics.stopped());
            }
            guard
                .as_ref()
                .cloned()
                .ok_or_else(|| self.metrics.stopped())?
        };
        let (completed, receiver) = mpsc::channel();
        sender
            .send(DurableControllerCommand::Barrier(completed))
            .map_err(|_| self.metrics.stopped())?;
        receiver.recv().map_err(|_| self.metrics.stopped())?
    }

    pub fn shutdown(&self) -> DurableShutdownReport {
        self.shutdown_internal()
    }

    pub fn metrics(&self) -> DurableQueuedControllerMetrics {
        self.metrics.snapshot(self.capacity, self.max_batch)
    }

    pub fn store_state(&self) -> FileEpochState {
        self.database
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .state()
    }

    pub fn store_metrics(&self) -> FileEpochMetrics {
        self.database
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .metrics()
    }
}

impl<I: EpochFileIo> DurableQueuedIntentController<I> {
    fn shutdown_internal(&self) -> DurableShutdownReport {
        let _shutdown_guard = self
            .shutdown_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous_state = self.metrics.state();
        if matches!(
            previous_state,
            DurableControllerState::Starting | DurableControllerState::Running
        ) {
            self.metrics.set_state(DurableControllerState::Draining);
        }
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(sender);

        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            if let Err(payload) = worker.join() {
                let reason = self.metrics.poison(format!(
                    "durable worker join observed panic: {}",
                    panic_message(payload)
                ));
                poison_store(&self.database, &reason);
            }
        }
        if self.metrics.state() == DurableControllerState::Draining {
            self.metrics.set_state(DurableControllerState::Closed);
        }
        self.writer_lease
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();

        DurableShutdownReport {
            previous_state,
            final_state: self.metrics.state(),
            queued_stopped: self.metrics.shutdown_before_claim.load(Ordering::Relaxed),
            worker_failures: self.metrics.worker_failed.load(Ordering::Relaxed),
            reason: self.metrics.reason(),
        }
    }
}

impl<I: EpochFileIo> Drop for DurableQueuedIntentController<I> {
    fn drop(&mut self) {
        let _ = self.shutdown_internal();
    }
}

fn run_durable_worker<I: EpochFileIo + 'static>(
    database: Arc<Database<FileEpochStore<I>>>,
    receiver: Receiver<DurableControllerCommand>,
    policy: BatchPolicy,
    metrics: Arc<DurableControllerMetricsInner>,
    sender: Arc<Mutex<Option<SyncSender<DurableControllerCommand>>>>,
) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        run_durable_worker_loop(&database, &receiver, policy, &metrics, &sender)
    }));
    if let Err(payload) = result {
        let reason = metrics.poison(format!(
            "durable worker panicked outside epoch boundary: {}",
            panic_message(payload)
        ));
        poison_store(&database, &reason);
        sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drain_receiver(&receiver, DrainReason::WorkerFailed(reason), &metrics);
    }
    if metrics.state() == DurableControllerState::Running {
        let reason = metrics.poison("durable worker exited while controller was running");
        poison_store(&database, &reason);
    } else if metrics.state() == DurableControllerState::Draining {
        metrics.set_state(DurableControllerState::Closed);
    }
    metrics.worker_alive.store(false, Ordering::Release);
}

#[cfg(target_os = "linux")]
type DurableRoute = (
    mpsc::Sender<DurableTicketOutcome>,
    Arc<DurableTicketLifecycle>,
);

#[cfg(target_os = "linux")]
struct UnplannedDurableBatch {
    intents: Vec<QueuedIntent>,
    routes: Vec<DurableRoute>,
    epoch_started: Instant,
}

#[cfg(target_os = "linux")]
struct PreparedDurableBatch {
    source: UnplannedDurableBatch,
    plan: EpochPlan,
}

#[cfg(target_os = "linux")]
fn run_speculative_io_uring_worker(
    database: Arc<Database<FileEpochStore<IoUringEpochFileIo>>>,
    receiver: Receiver<DurableControllerCommand>,
    policy: BatchPolicy,
    metrics: Arc<DurableControllerMetricsInner>,
    sender: Arc<Mutex<Option<SyncSender<DurableControllerCommand>>>>,
) {
    let max_batch = match policy {
        BatchPolicy::ImmediateDrain { max_batch }
        | BatchPolicy::Coalesce { max_batch, .. }
        | BatchPolicy::Adaptive { max_batch, .. } => max_batch,
    };
    let result = catch_unwind(AssertUnwindSafe(|| {
        run_speculative_io_uring_worker_loop(&database, &receiver, max_batch, &metrics, &sender)
    }));
    if let Err(payload) = result {
        let reason = metrics.poison(format!(
            "speculative durability worker panicked: {}",
            panic_message(payload)
        ));
        metrics.worker_failed.fetch_add(1, Ordering::Relaxed);
        poison_store(&database, &reason);
        sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drain_receiver(&receiver, DrainReason::WorkerFailed(reason), &metrics);
    }
    if metrics.state() == DurableControllerState::Running {
        let reason =
            metrics.poison("speculative durability worker exited while controller was running");
        poison_store(&database, &reason);
    } else if metrics.state() == DurableControllerState::Draining {
        metrics.set_state(DurableControllerState::Closed);
    }
    metrics.worker_alive.store(false, Ordering::Release);
}

#[cfg(target_os = "linux")]
fn run_speculative_io_uring_worker_loop(
    database: &Arc<Database<FileEpochStore<IoUringEpochFileIo>>>,
    receiver: &Receiver<DurableControllerCommand>,
    max_batch: usize,
    metrics: &Arc<DurableControllerMetricsInner>,
    sender: &Arc<Mutex<Option<SyncSender<DurableControllerCommand>>>>,
) {
    let mut pending = VecDeque::new();
    loop {
        let command = match pending.pop_front() {
            Some(command) => command,
            None => match receiver.recv() {
                Ok(command) => command,
                Err(_) => break,
            },
        };

        match metrics.state() {
            DurableControllerState::Draining | DurableControllerState::Closed => {
                reject_command(command, DrainReason::Shutdown, metrics);
                drain_pending_and_receiver(&mut pending, receiver, DrainReason::Shutdown, metrics);
                break;
            }
            DurableControllerState::Poisoned => {
                let reason = metrics
                    .reason()
                    .unwrap_or_else(|| "durable controller is poisoned".to_owned());
                reject_command(command, DrainReason::WorkerFailed(reason.clone()), metrics);
                drain_pending_and_receiver(
                    &mut pending,
                    receiver,
                    DrainReason::WorkerFailed(reason),
                    metrics,
                );
                break;
            }
            DurableControllerState::Starting | DurableControllerState::Running => {}
        }

        match command {
            DurableControllerCommand::Intent(first) => {
                let batch = claim_batch(first, receiver, &mut pending, max_batch, metrics);
                if let Err(reason) = process_speculative_pipeline(
                    database,
                    batch,
                    receiver,
                    &mut pending,
                    max_batch,
                    metrics,
                ) {
                    let reason = metrics.poison(reason);
                    poison_store(database, &reason);
                    sender
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .take();
                    drain_pending_and_receiver(
                        &mut pending,
                        receiver,
                        DrainReason::WorkerFailed(reason),
                        metrics,
                    );
                    break;
                }
            }
            DurableControllerCommand::Barrier(completed) => {
                let _ = completed.send(Ok(()));
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn claim_batch(
    first: DurableStagedIntent,
    receiver: &Receiver<DurableControllerCommand>,
    pending: &mut VecDeque<DurableControllerCommand>,
    max_batch: usize,
    metrics: &DurableControllerMetricsInner,
) -> Vec<DurableStagedIntent> {
    let mut batch = Vec::with_capacity(max_batch);
    claim(first, metrics, &mut batch);
    while batch.len() < max_batch && metrics.state() == DurableControllerState::Running {
        match receiver.try_recv() {
            Ok(DurableControllerCommand::Intent(staged)) => claim(staged, metrics, &mut batch),
            Ok(command @ DurableControllerCommand::Barrier(_)) => {
                pending.push_back(command);
                break;
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
    batch
}

#[cfg(target_os = "linux")]
fn try_claim_successor_batch(
    receiver: &Receiver<DurableControllerCommand>,
    pending: &mut VecDeque<DurableControllerCommand>,
    max_batch: usize,
    metrics: &DurableControllerMetricsInner,
) -> Option<Vec<DurableStagedIntent>> {
    if metrics.state() != DurableControllerState::Running {
        return None;
    }
    match receiver.try_recv() {
        Ok(DurableControllerCommand::Intent(first)) => {
            Some(claim_batch(first, receiver, pending, max_batch, metrics))
        }
        Ok(command @ DurableControllerCommand::Barrier(_)) => {
            pending.push_back(command);
            None
        }
        Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
    }
}

#[cfg(target_os = "linux")]
fn unplan_batch(
    batch: Vec<DurableStagedIntent>,
    metrics: &DurableControllerMetricsInner,
) -> UnplannedDurableBatch {
    metrics
        .in_flight
        .fetch_add(batch.len() as u64, Ordering::AcqRel);
    let mut intents = Vec::with_capacity(batch.len());
    let mut routes = Vec::with_capacity(batch.len());
    for staged in batch {
        intents.push(staged.intent);
        routes.push((staged.completion, staged.lifecycle));
    }
    UnplannedDurableBatch {
        intents,
        routes,
        epoch_started: Instant::now(),
    }
}

#[cfg(target_os = "linux")]
fn derive_prepared_batch(
    source: UnplannedDurableBatch,
    base: Arc<World>,
    validators: &[Validator],
    metrics: &DurableControllerMetricsInner,
) -> Result<PreparedDurableBatch, (UnplannedDurableBatch, String)> {
    let derive_started = Instant::now();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut materializer = VmEpochMaterializer::new(base.next_entity());
        materializer
            .materialize(base, source.intents.clone(), validators)
            .map(|(plan, _)| plan)
            .expect("vm materialize")
    }));
    metrics
        .derive_nanos
        .fetch_add(nanos(derive_started.elapsed()), Ordering::Relaxed);
    match result {
        Ok(plan) => Ok(PreparedDurableBatch { source, plan }),
        Err(payload) => Err((
            source,
            format!(
                "speculative epoch derivation failed: {}",
                panic_message(payload)
            ),
        )),
    }
}

#[cfg(target_os = "linux")]
fn process_speculative_pipeline(
    database: &Database<FileEpochStore<IoUringEpochFileIo>>,
    first: Vec<DurableStagedIntent>,
    receiver: &Receiver<DurableControllerCommand>,
    pending_commands: &mut VecDeque<DurableControllerCommand>,
    max_batch: usize,
    metrics: &DurableControllerMetricsInner,
) -> Result<(), String> {
    let _commit_guard = database
        .commit_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let validators = database
        .validators
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let source = unplan_batch(first, metrics);
    let mut prepared =
        match derive_prepared_batch(source, database.snapshot(), &validators, metrics) {
            Ok(prepared) => prepared,
            Err((source, reason)) => {
                finish_stopped_batch(source, &reason, metrics);
                return Err(reason);
            }
        };

    loop {
        if prepared.plan.is_empty() {
            publish_and_route_success(database, prepared, metrics);
            let Some(batch) =
                try_claim_successor_batch(receiver, pending_commands, max_batch, metrics)
            else {
                return Ok(());
            };
            let source = unplan_batch(batch, metrics);
            prepared =
                match derive_prepared_batch(source, database.snapshot(), &validators, metrics) {
                    Ok(prepared) => prepared,
                    Err((source, reason)) => {
                        finish_stopped_batch(source, &reason, metrics);
                        return Err(reason);
                    }
                };
            continue;
        }

        let mut store = database
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let file_epoch = match store.prepare_epoch(prepared.plan.frames()) {
            Ok(file_epoch) => file_epoch,
            Err(error) => {
                drop(store);
                finish_failed_batch(prepared, &error, metrics);
                return Ok(());
            }
        };
        let persist_started = Instant::now();
        let pending_io = match store
            .io_mut()
            .submit_contiguous_epoch(file_epoch.start_offset(), file_epoch.records())
        {
            Ok(pending_io) => pending_io,
            Err(error) => {
                let result = store.finish_prepared_epoch(file_epoch, Err(error));
                drop(store);
                match result {
                    Ok(()) => unreachable!("failed submission cannot finish successfully"),
                    Err(error) => finish_failed_batch(prepared, &error, metrics),
                }
                return Ok(());
            }
        };

        // Only validation and private world construction overlap the kernel's
        // WRITE + DATASYNC. The file store and commit lock remain exclusively
        // owned here; no successor bytes are submitted before this completion.
        let successor = try_claim_successor_batch(receiver, pending_commands, max_batch, metrics)
            .map(|batch| {
                metrics
                    .speculative_epochs_prepared
                    .fetch_add(1, Ordering::Relaxed);
                let source = unplan_batch(batch, metrics);
                derive_prepared_batch(source, prepared.plan.tail(), &validators, metrics)
            });

        let (transport_metrics, io_result) = store.io_mut().complete_contiguous_epoch(pending_io);
        store.record_transport_metrics(transport_metrics);
        let persist_result = store.finish_prepared_epoch(file_epoch, io_result);
        metrics
            .persist_nanos
            .fetch_add(nanos(persist_started.elapsed()), Ordering::Relaxed);
        drop(store);

        let persisted = persist_result.is_ok();
        match persist_result {
            Ok(()) => publish_and_route_success(database, prepared, metrics),
            Err(error) => finish_failed_batch(prepared, &error, metrics),
        }

        let Some(successor) = successor else {
            return Ok(());
        };
        prepared = match successor {
            Ok(prepared) if persisted => prepared,
            Ok(prepared) => {
                metrics
                    .speculative_epochs_rederived
                    .fetch_add(1, Ordering::Relaxed);
                let source = prepared.source;
                match derive_prepared_batch(source, database.snapshot(), &validators, metrics) {
                    Ok(prepared) => prepared,
                    Err((source, reason)) => {
                        finish_stopped_batch(source, &reason, metrics);
                        return Err(reason);
                    }
                }
            }
            Err((source, reason)) => {
                finish_stopped_batch(source, &reason, metrics);
                return Err(reason);
            }
        };
    }
}

#[cfg(target_os = "linux")]
fn publish_and_route_success(
    database: &Database<FileEpochStore<IoUringEpochFileIo>>,
    prepared: PreparedDurableBatch,
    metrics: &DurableControllerMetricsInner,
) {
    let publish_started = Instant::now();
    *database
        .current
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = prepared.plan.tail();
    metrics
        .publish_nanos
        .fetch_add(nanos(publish_started.elapsed()), Ordering::Relaxed);
    let delivery_started = Instant::now();
    route_success(prepared.source.routes, &prepared.plan, metrics);
    finish_batch_metrics(
        prepared.source.intents.len(),
        prepared.source.epoch_started,
        delivery_started,
        metrics,
    );
}

#[cfg(target_os = "linux")]
fn finish_failed_batch(
    prepared: PreparedDurableBatch,
    error: &FileEpochStoreError,
    metrics: &DurableControllerMetricsInner,
) {
    let delivery_started = Instant::now();
    route_failure(prepared.source.routes, &prepared.plan, error, metrics);
    finish_batch_metrics(
        prepared.source.intents.len(),
        prepared.source.epoch_started,
        delivery_started,
        metrics,
    );
}

#[cfg(target_os = "linux")]
fn finish_stopped_batch(
    source: UnplannedDurableBatch,
    reason: &str,
    metrics: &DurableControllerMetricsInner,
) {
    let delivery_started = Instant::now();
    route_stopped(
        source.routes,
        DurableTicketStopReason::WorkerFailed(reason.to_owned()),
        metrics,
    );
    metrics.worker_failed.fetch_add(1, Ordering::Relaxed);
    finish_batch_metrics(
        source.intents.len(),
        source.epoch_started,
        delivery_started,
        metrics,
    );
}

#[cfg(target_os = "linux")]
fn finish_batch_metrics(
    intents: usize,
    epoch_started: Instant,
    delivery_started: Instant,
    metrics: &DurableControllerMetricsInner,
) {
    metrics
        .delivery_nanos
        .fetch_add(nanos(delivery_started.elapsed()), Ordering::Relaxed);
    metrics.epochs.fetch_add(1, Ordering::Relaxed);
    metrics
        .in_flight
        .fetch_sub(intents as u64, Ordering::AcqRel);
    metrics
        .epoch_total_nanos
        .fetch_add(nanos(epoch_started.elapsed()), Ordering::Relaxed);
}

fn run_durable_worker_loop<I: EpochFileIo + 'static>(
    database: &Arc<Database<FileEpochStore<I>>>,
    receiver: &Receiver<DurableControllerCommand>,
    policy: BatchPolicy,
    metrics: &Arc<DurableControllerMetricsInner>,
    sender: &Arc<Mutex<Option<SyncSender<DurableControllerCommand>>>>,
) {
    let mut pending = VecDeque::new();
    let (min_batch, max_batch) = match policy {
        BatchPolicy::ImmediateDrain { max_batch } => (1, max_batch),
        BatchPolicy::Coalesce { max_batch, .. } => (1, max_batch),
        BatchPolicy::Adaptive {
            min_batch,
            max_batch,
            ..
        } => (min_batch, max_batch),
    };
    let mut state = AdaptiveState::new(min_batch.max(16).min(max_batch));

    loop {
        let command = match pending.pop_front() {
            Some(command) => command,
            None => match receiver.recv() {
                Ok(command) => command,
                Err(_) => break,
            },
        };

        match metrics.state() {
            DurableControllerState::Draining | DurableControllerState::Closed => {
                reject_command(command, DrainReason::Shutdown, metrics);
                drain_pending_and_receiver(&mut pending, receiver, DrainReason::Shutdown, metrics);
                break;
            }
            DurableControllerState::Poisoned => {
                let reason = metrics
                    .reason()
                    .unwrap_or_else(|| "durable controller is poisoned".to_owned());
                reject_command(command, DrainReason::WorkerFailed(reason.clone()), metrics);
                drain_pending_and_receiver(
                    &mut pending,
                    receiver,
                    DrainReason::WorkerFailed(reason),
                    metrics,
                );
                break;
            }
            DurableControllerState::Starting | DurableControllerState::Running => {}
        }

        match command {
            DurableControllerCommand::Intent(first) => {
                let mut batch = Vec::with_capacity(max_batch);
                let mut oldest_enqueued_at = first.admitted_at;
                let enqueued_at = first.admitted_at;
                claim(first, metrics, &mut batch);
                state.observe_arrival(enqueued_at);

                let seal_reason = match policy {
                    BatchPolicy::ImmediateDrain { .. } => {
                        while batch.len() < max_batch
                            && metrics.state() == DurableControllerState::Running
                        {
                            match receiver.try_recv() {
                                Ok(DurableControllerCommand::Intent(staged)) => {
                                    let enqueued_at = staged.admitted_at;
                                    oldest_enqueued_at = oldest_enqueued_at.min(enqueued_at);
                                    claim(staged, metrics, &mut batch);
                                    state.observe_arrival(enqueued_at);
                                }
                                Ok(command @ DurableControllerCommand::Barrier(_)) => {
                                    pending.push_back(command);
                                    break;
                                }
                                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                            }
                        }
                        if batch.len() >= max_batch {
                            BatchSealReason::Capacity
                        } else {
                            BatchSealReason::Drain
                        }
                    }
                    BatchPolicy::Coalesce { max_delay, .. } => {
                        let deadline = Instant::now() + max_delay;
                        while batch.len() < max_batch
                            && metrics.state() == DurableControllerState::Running
                        {
                            let now = Instant::now();
                            if now >= deadline {
                                break;
                            }
                            let remaining = deadline - now;
                            match receiver.recv_timeout(remaining) {
                                Ok(DurableControllerCommand::Intent(staged)) => {
                                    let enqueued_at = staged.admitted_at;
                                    oldest_enqueued_at = oldest_enqueued_at.min(enqueued_at);
                                    claim(staged, metrics, &mut batch);
                                    state.observe_arrival(enqueued_at);
                                }
                                Ok(command @ DurableControllerCommand::Barrier(_)) => {
                                    pending.push_back(command);
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        if batch.len() >= max_batch {
                            BatchSealReason::Capacity
                        } else {
                            BatchSealReason::Timeout
                        }
                    }
                    BatchPolicy::Adaptive { latency_budget, .. } => {
                        while batch.len() < state.target_width
                            && metrics.state() == DurableControllerState::Running
                        {
                            match receiver.try_recv() {
                                Ok(DurableControllerCommand::Intent(staged)) => {
                                    let enqueued_at = staged.admitted_at;
                                    oldest_enqueued_at = oldest_enqueued_at.min(enqueued_at);
                                    claim(staged, metrics, &mut batch);
                                    state.observe_arrival(enqueued_at);
                                }
                                Ok(command @ DurableControllerCommand::Barrier(_)) => {
                                    pending.push_back(command);
                                    break;
                                }
                                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                            }
                        }

                        let mut reason = if batch.len() >= state.target_width {
                            BatchSealReason::Width
                        } else {
                            let now = Instant::now();
                            let age = now.saturating_duration_since(oldest_enqueued_at);
                            if age >= latency_budget {
                                BatchSealReason::Latency
                            } else {
                                let remaining_budget = latency_budget - age;
                                let predicted_interarrival =
                                    Duration::from_nanos(state.ewma_interarrival_time_ns as u64);
                                let probe_wait = (predicted_interarrival * 2)
                                    .clamp(Duration::from_micros(10), Duration::from_micros(250))
                                    .min(remaining_budget);

                                metrics
                                    .total_adaptive_probe_wait_ns
                                    .fetch_add(probe_wait.as_nanos() as u64, Ordering::Relaxed);

                                match receiver.recv_timeout(probe_wait) {
                                    Ok(DurableControllerCommand::Intent(staged)) => {
                                        let enqueued_at = staged.admitted_at;
                                        oldest_enqueued_at = oldest_enqueued_at.min(enqueued_at);
                                        claim(staged, metrics, &mut batch);
                                        state.observe_arrival(enqueued_at);

                                        while batch.len() < state.target_width
                                            && metrics.state() == DurableControllerState::Running
                                        {
                                            match receiver.try_recv() {
                                                Ok(DurableControllerCommand::Intent(staged)) => {
                                                    let enqueued_at = staged.admitted_at;
                                                    oldest_enqueued_at =
                                                        oldest_enqueued_at.min(enqueued_at);
                                                    claim(staged, metrics, &mut batch);
                                                    state.observe_arrival(enqueued_at);
                                                }
                                                Ok(
                                                    command @ DurableControllerCommand::Barrier(_),
                                                ) => {
                                                    pending.push_back(command);
                                                    break;
                                                }
                                                Err(TryRecvError::Empty)
                                                | Err(TryRecvError::Disconnected) => break,
                                            }
                                        }

                                        if batch.len() >= state.target_width {
                                            BatchSealReason::Width
                                        } else {
                                            BatchSealReason::LowTraffic
                                        }
                                    }
                                    Ok(command @ DurableControllerCommand::Barrier(_)) => {
                                        pending.push_back(command);
                                        BatchSealReason::Barrier
                                    }
                                    Err(_) => {
                                        if predicted_interarrival > latency_budget / 2 {
                                            BatchSealReason::LowTraffic
                                        } else {
                                            BatchSealReason::SourceStalled
                                        }
                                    }
                                }
                            }
                        };

                        if batch.len() >= max_batch {
                            reason = BatchSealReason::Capacity;
                        }

                        let seal_age = Instant::now().saturating_duration_since(oldest_enqueued_at);
                        metrics.record_oldest_age_at_seal(seal_age);
                        state.update_target(reason, batch.len(), min_batch, max_batch);
                        metrics.record_max_target_width(state.target_width);
                        reason
                    }
                };

                metrics.record_seal_reason(seal_reason);

                if let Err(reason) = process_durable_batch(database, batch, metrics) {
                    let reason = metrics.poison(reason);
                    poison_store(database, &reason);
                    sender
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .take();
                    drain_pending_and_receiver(
                        &mut pending,
                        receiver,
                        DrainReason::WorkerFailed(reason),
                        metrics,
                    );
                    break;
                }
            }
            DurableControllerCommand::Barrier(completed) => {
                metrics.record_seal_reason(BatchSealReason::Barrier);
                let _ = completed.send(Ok(()));
            }
        }
    }
}

fn claim(
    staged: DurableStagedIntent,
    metrics: &DurableControllerMetricsInner,
    batch: &mut Vec<DurableStagedIntent>,
) {
    metrics.release_ingress();
    metrics.claimed.fetch_add(1, Ordering::Relaxed);
    metrics
        .queue_wait_nanos
        .fetch_add(nanos(staged.admitted_at.elapsed()), Ordering::Relaxed);
    staged.lifecycle.claim();
    batch.push(staged);
}

struct DurableEpochFailure {
    plan: EpochPlan,
    error: FileEpochStoreError,
}

fn commit_durable_epoch<I: EpochFileIo>(
    database: &Database<FileEpochStore<I>>,
    intents: Vec<QueuedIntent>,
    metrics: &DurableControllerMetricsInner,
) -> Result<EpochPlan, DurableEpochFailure> {
    let _commit_guard = database
        .commit_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let derive_started = Instant::now();
    let base = database.snapshot();
    let validators = database
        .validators
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let mut materializer = VmEpochMaterializer::new(base.next_entity());
    let plan = materializer
        .materialize(base, intents, &validators)
        .expect("vm materialize")
        .0;
    metrics
        .derive_nanos
        .fetch_add(nanos(derive_started.elapsed()), Ordering::Relaxed);
    lifecycle_fault_point("after_derive_before_persist");
    if plan.is_empty() {
        return Ok(plan);
    }

    let persist_started = Instant::now();
    let append_result = database
        .store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .append_epoch(plan.frames());
    metrics
        .persist_nanos
        .fetch_add(nanos(persist_started.elapsed()), Ordering::Relaxed);
    if let Err(error) = append_result {
        return Err(DurableEpochFailure { plan, error });
    }
    lifecycle_fault_point("after_persist_before_publish");

    let publish_started = Instant::now();
    *database
        .current
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = plan.tail();
    metrics
        .publish_nanos
        .fetch_add(nanos(publish_started.elapsed()), Ordering::Relaxed);
    lifecycle_fault_point("after_publish_before_delivery");
    Ok(plan)
}

fn process_durable_batch<I: EpochFileIo>(
    database: &Database<FileEpochStore<I>>,
    batch: Vec<DurableStagedIntent>,
    metrics: &DurableControllerMetricsInner,
) -> Result<(), String> {
    let epoch_started = Instant::now();
    metrics
        .in_flight
        .store(batch.len() as u64, Ordering::Release);
    let mut intents = Vec::with_capacity(batch.len());
    let mut routes = Vec::with_capacity(batch.len());
    for staged in batch {
        intents.push(staged.intent);
        routes.push((staged.completion, staged.lifecycle));
    }

    let commit = catch_unwind(AssertUnwindSafe(|| {
        commit_durable_epoch(database, intents, metrics)
    }));
    let delivery_started = Instant::now();
    match commit {
        Ok(Ok(plan)) => route_success(routes, &plan, metrics),
        Ok(Err(failure)) => route_failure(routes, &failure.plan, &failure.error, metrics),
        Err(payload) => {
            let reason = format!("durable epoch worker failed: {}", panic_message(payload));
            route_stopped(
                routes,
                DurableTicketStopReason::WorkerFailed(reason.clone()),
                metrics,
            );
            metrics.worker_failed.fetch_add(1, Ordering::Relaxed);
            metrics.in_flight.store(0, Ordering::Release);
            metrics
                .delivery_nanos
                .fetch_add(nanos(delivery_started.elapsed()), Ordering::Relaxed);
            metrics
                .epoch_total_nanos
                .fetch_add(nanos(epoch_started.elapsed()), Ordering::Relaxed);
            return Err(reason);
        }
    }
    metrics
        .delivery_nanos
        .fetch_add(nanos(delivery_started.elapsed()), Ordering::Relaxed);
    metrics.epochs.fetch_add(1, Ordering::Relaxed);
    metrics.in_flight.store(0, Ordering::Release);
    metrics
        .epoch_total_nanos
        .fetch_add(nanos(epoch_started.elapsed()), Ordering::Relaxed);
    Ok(())
}

fn route_success(
    routes: Vec<(
        mpsc::Sender<DurableTicketOutcome>,
        Arc<DurableTicketLifecycle>,
    )>,
    plan: &EpochPlan,
    metrics: &DurableControllerMetricsInner,
) {
    for ((completion, lifecycle), outcome) in routes.into_iter().zip(plan.outcomes()) {
        let ticket_outcome = match outcome {
            EpochOutcome::Accepted(accepted) => {
                metrics.accepted.fetch_add(1, Ordering::Relaxed);
                DurableTicketOutcome::Accepted {
                    world: accepted.world(),
                    frame: accepted.frame(),
                    entities: accepted.entities().clone(),
                }
            }
            EpochOutcome::Rejected(rejected) => {
                metrics.rejected.fetch_add(1, Ordering::Relaxed);
                DurableTicketOutcome::Rejected(DurableTicketRejection::from_intent(
                    rejected.error(),
                ))
            }
        };
        resolve(completion, lifecycle, ticket_outcome, metrics);
    }
}

fn route_failure(
    routes: Vec<(
        mpsc::Sender<DurableTicketOutcome>,
        Arc<DurableTicketLifecycle>,
    )>,
    plan: &EpochPlan,
    error: &FileEpochStoreError,
    metrics: &DurableControllerMetricsInner,
) {
    let durability_message = error.to_string();
    for ((completion, lifecycle), outcome) in routes.into_iter().zip(plan.outcomes()) {
        let ticket_outcome = match outcome {
            EpochOutcome::Accepted(_) => {
                metrics.durability_failed.fetch_add(1, Ordering::Relaxed);
                DurableTicketOutcome::DurabilityFailed(durability_message.clone())
            }
            EpochOutcome::Rejected(rejected) => {
                metrics.rejected.fetch_add(1, Ordering::Relaxed);
                DurableTicketOutcome::Rejected(DurableTicketRejection::from_intent(
                    rejected.error(),
                ))
            }
        };
        resolve(completion, lifecycle, ticket_outcome, metrics);
    }
}

fn route_stopped(
    routes: Vec<(
        mpsc::Sender<DurableTicketOutcome>,
        Arc<DurableTicketLifecycle>,
    )>,
    reason: DurableTicketStopReason,
    metrics: &DurableControllerMetricsInner,
) {
    for (completion, lifecycle) in routes {
        resolve(
            completion,
            lifecycle,
            DurableTicketOutcome::Stopped(reason.clone()),
            metrics,
        );
    }
}

#[derive(Clone)]
enum DrainReason {
    Shutdown,
    WorkerFailed(String),
}

fn reject_command(
    command: DurableControllerCommand,
    reason: DrainReason,
    metrics: &DurableControllerMetricsInner,
) {
    match command {
        DurableControllerCommand::Intent(staged) => {
            metrics.release_ingress();
            let outcome = match reason {
                DrainReason::Shutdown => {
                    metrics
                        .shutdown_before_claim
                        .fetch_add(1, Ordering::Relaxed);
                    DurableTicketOutcome::Stopped(DurableTicketStopReason::ShutdownBeforeClaim)
                }
                DrainReason::WorkerFailed(message) => {
                    metrics.worker_failed.fetch_add(1, Ordering::Relaxed);
                    DurableTicketOutcome::Stopped(DurableTicketStopReason::WorkerFailed(message))
                }
            };
            resolve(staged.completion, staged.lifecycle, outcome, metrics);
        }
        DurableControllerCommand::Barrier(completed) => {
            let _ = completed.send(Err(metrics.stopped()));
        }
    }
}

fn drain_pending_and_receiver(
    pending: &mut VecDeque<DurableControllerCommand>,
    receiver: &Receiver<DurableControllerCommand>,
    reason: DrainReason,
    metrics: &DurableControllerMetricsInner,
) {
    while let Some(command) = pending.pop_front() {
        reject_command(command, reason.clone(), metrics);
    }
    drain_receiver(receiver, reason, metrics);
}

fn drain_receiver(
    receiver: &Receiver<DurableControllerCommand>,
    reason: DrainReason,
    metrics: &DurableControllerMetricsInner,
) {
    loop {
        match receiver.try_recv() {
            Ok(command) => reject_command(command, reason.clone(), metrics),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
}

fn resolve(
    completion: mpsc::Sender<DurableTicketOutcome>,
    lifecycle: Arc<DurableTicketLifecycle>,
    outcome: DurableTicketOutcome,
    metrics: &DurableControllerMetricsInner,
) {
    lifecycle.resolve();
    if completion.send(outcome).is_err() {
        metrics
            .completion_delivery_failures
            .fetch_add(1, Ordering::Relaxed);
    }
}

fn poison_store<I: EpochFileIo>(database: &Database<FileEpochStore<I>>, reason: &str) {
    database
        .store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .poison_external(reason.to_owned());
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

fn nanos(duration: std::time::Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

#[cfg(feature = "fault-injection")]
fn lifecycle_fault_point(name: &str) {
    if std::env::var("FORTHDB_M6D_CRASH_POINT").ok().as_deref() == Some(name) {
        std::process::exit(86);
    }
}

#[cfg(not(feature = "fault-injection"))]
fn lifecycle_fault_point(_name: &str) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct FailFirstEpochSyncIo {
        inner: StdEpochFileIo,
        fail: AtomicBool,
    }

    impl FailFirstEpochSyncIo {
        fn open(path: &Path) -> Self {
            Self {
                inner: StdEpochFileIo::open(path).expect("file opens"),
                fail: AtomicBool::new(true),
            }
        }
    }

    impl EpochFileIo for FailFirstEpochSyncIo {
        fn len(&mut self, phase: EpochIoPhase) -> std::io::Result<u64> {
            self.inner.len(phase)
        }

        fn write_at(
            &mut self,
            phase: EpochIoPhase,
            offset: u64,
            bytes: &[u8],
        ) -> std::io::Result<usize> {
            self.inner.write_at(phase, offset, bytes)
        }

        fn sync_data(&mut self, phase: EpochIoPhase) -> std::io::Result<()> {
            if phase == EpochIoPhase::EpochSync && self.fail.swap(false, Ordering::SeqCst) {
                return Err(std::io::Error::from_raw_os_error(5));
            }
            self.inner.sync_data(phase)
        }

        fn set_len(&mut self, phase: EpochIoPhase, len: u64) -> std::io::Result<()> {
            self.inner.set_len(phase, len)
        }

        fn read_all(&mut self, phase: EpochIoPhase) -> std::io::Result<Vec<u8>> {
            self.inner.read_all(phase)
        }
    }

    fn temp_path(label: &str) -> PathBuf {
        let sequence = PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "forthdb-durable-controller-{label}-{}-{sequence}.db",
            std::process::id()
        ))
    }

    fn state_fact(value: &str) -> Fact {
        Fact::new(
            Atom::Entity(EntityId::new(1)),
            Predicate::new("state"),
            Atom::Literal(Literal::new(value)),
        )
    }

    fn intent(slot: &str, value: &str) -> QueuedIntent {
        let mut intent = QueuedIntent::new();
        intent.define_fact(SlotId::new(slot), state_fact(value));
        intent
    }

    #[test]
    fn accepted_ticket_arrives_only_after_file_epoch_and_head_publication() {
        let path = temp_path("success");
        let store =
            FileEpochStore::open(&path, FileEpochSyncPolicy::PerEpoch).expect("epoch store opens");
        let database = Arc::new(Database::new(store).expect("database opens"));
        let controller =
            DurableQueuedIntentController::new(database.clone(), 16, 8).expect("controller starts");
        let ticket = controller
            .submit(intent("durable/state", "ready"))
            .expect("intent admitted");
        let world = ticket
            .wait()
            .expect("ticket resolves")
            .world()
            .expect("ticket accepted");
        assert_eq!(world.version(), 1);
        assert_eq!(database.snapshot().id(), world.id());
        assert_eq!(database.frame_count(), 1);
        assert_eq!(controller.store_metrics().data_syncs, 1);
        let reopened = FileCommitStore::open(&path).expect("file reopens");
        assert_eq!(reopened.len(), 1);
        drop(controller);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn repaired_epoch_failure_publishes_nothing_and_next_epoch_can_succeed() {
        let path = temp_path("repair");
        FileCommitStore::open(&path).expect("file initializes");
        let io = FailFirstEpochSyncIo::open(&path);
        let store = FileEpochStore::from_io(&path, io, FileEpochSyncPolicy::PerEpoch)
            .expect("injected store opens");
        let database = Arc::new(Database::new(store).expect("database opens"));
        let controller =
            DurableQueuedIntentController::new(database.clone(), 16, 8).expect("controller starts");

        let failed = controller
            .submit(intent("durable/failed", "not-published"))
            .expect("intent admitted")
            .wait()
            .expect("ticket resolves");
        assert!(matches!(failed, DurableTicketOutcome::DurabilityFailed(_)));
        assert_eq!(database.snapshot().version(), 0);
        assert_eq!(database.frame_count(), 0);
        assert_eq!(controller.store_state(), FileEpochState::Healthy);

        let succeeded = controller
            .submit(intent("durable/success", "published"))
            .expect("second admitted")
            .wait()
            .expect("second resolves");
        assert_eq!(succeeded.world().expect("accepted").version(), 1);
        assert_eq!(database.snapshot().version(), 1);
        assert_eq!(database.frame_count(), 1);
        let metrics = controller.metrics();
        assert_eq!(metrics.durability_failed, 1);
        assert_eq!(metrics.accepted, 1);
        drop(controller);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn semantic_rejection_remains_independent_of_neighboring_durability() {
        let path = temp_path("rejection");
        let store =
            FileEpochStore::open(&path, FileEpochSyncPolicy::PerEpoch).expect("epoch store opens");
        let database = Arc::new(Database::new(store).expect("database opens"));
        let controller =
            DurableQueuedIntentController::new(database.clone(), 16, 8).expect("controller starts");

        let accepted = controller
            .submit(intent("durable/accepted", "yes"))
            .expect("accepted admitted");
        let mut rejected_intent = intent("durable/rejected", "no");
        rejected_intent.expect_world(WorldId::new(123));
        let rejected = controller
            .submit(rejected_intent)
            .expect("rejected admitted");

        assert!(matches!(
            accepted.wait().expect("accepted resolves"),
            DurableTicketOutcome::Accepted { .. }
        ));
        assert!(matches!(
            rejected.wait().expect("rejected resolves"),
            DurableTicketOutcome::Rejected(_)
        ));
        assert_eq!(database.snapshot().version(), 1);
        assert_eq!(database.frame_count(), 1);
        drop(controller);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn graceful_shutdown_finishes_claimed_epoch_and_stops_queued_intents() {
        use std::sync::Condvar;
        use std::time::{Duration, Instant};

        let path = temp_path("graceful-shutdown");
        let store =
            FileEpochStore::open(&path, FileEpochSyncPolicy::PerEpoch).expect("epoch store opens");
        let database = Arc::new(Database::new(store).expect("database opens"));
        let entered = Arc::new((std::sync::Mutex::new(false), Condvar::new()));
        let release = Arc::new((std::sync::Mutex::new(false), Condvar::new()));
        let validator_entered = entered.clone();
        let validator_release = release.clone();
        database.register_validator(move |_| {
            let (lock, signal) = &*validator_entered;
            *lock.lock().expect("entered lock") = true;
            signal.notify_all();
            let (lock, signal) = &*validator_release;
            let mut released = lock.lock().expect("release lock");
            while !*released {
                released = signal.wait(released).expect("release wait");
            }
            Ok(())
        });

        let controller = Arc::new(
            DurableQueuedIntentController::new(database.clone(), 16, 1).expect("controller starts"),
        );
        let first = controller
            .submit(intent("durable/claimed", "yes"))
            .expect("first admitted");

        let (lock, signal) = &*entered;
        let entered_deadline = Instant::now() + Duration::from_secs(5);
        let mut observed = lock.lock().expect("entered lock");
        while !*observed {
            let remaining = entered_deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "validator was never entered");
            let (next, timeout) = signal
                .wait_timeout(observed, remaining)
                .expect("entered wait");
            observed = next;
            assert!(
                !timeout.timed_out() || *observed,
                "validator entry timed out"
            );
        }
        drop(observed);

        let second = controller
            .submit(intent("durable/queued", "no"))
            .expect("second admitted");
        let shutdown_controller = controller.clone();
        let shutdown = thread::spawn(move || shutdown_controller.shutdown());

        let state_deadline = Instant::now() + Duration::from_secs(5);
        while controller.state() != DurableControllerState::Draining {
            assert!(
                Instant::now() < state_deadline,
                "controller never began draining"
            );
            thread::yield_now();
        }

        let (lock, signal) = &*release;
        *lock.lock().expect("release lock") = true;
        signal.notify_all();

        assert!(matches!(
            first.wait().expect("claimed ticket resolves"),
            DurableTicketOutcome::Accepted { .. }
        ));
        assert!(matches!(
            second.wait().expect("queued ticket resolves"),
            DurableTicketOutcome::Stopped(DurableTicketStopReason::ShutdownBeforeClaim)
        ));
        let report = shutdown.join().expect("shutdown joins");
        assert_eq!(report.final_state, DurableControllerState::Closed);
        assert_eq!(report.queued_stopped, 1);
        assert!(matches!(
            controller.submit(intent("durable/late", "no")),
            Err(DurableSubmitError::Closed(_))
        ));
        assert_eq!(database.snapshot().version(), 1);
        assert_eq!(database.frame_count(), 1);
        drop(controller);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn validator_panic_poison_fences_store_and_resolves_ticket() {
        use std::time::{Duration, Instant};

        let path = temp_path("worker-panic");
        let store =
            FileEpochStore::open(&path, FileEpochSyncPolicy::PerEpoch).expect("epoch store opens");
        let database = Arc::new(Database::new(store).expect("database opens"));
        database.register_validator(|_| panic!("injected validator panic"));
        let controller =
            DurableQueuedIntentController::new(database.clone(), 16, 1).expect("controller starts");
        let outcome = controller
            .submit(intent("durable/panic", "never"))
            .expect("intent admitted")
            .wait()
            .expect("ticket receives explicit worker outcome");
        assert!(matches!(
            outcome,
            DurableTicketOutcome::Stopped(DurableTicketStopReason::WorkerFailed(_))
        ));

        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.metrics().worker_alive {
            assert!(Instant::now() < deadline, "worker did not terminate");
            thread::yield_now();
        }
        assert_eq!(controller.state(), DurableControllerState::Poisoned);
        assert_eq!(controller.store_state(), FileEpochState::Poisoned);
        assert!(matches!(
            controller.submit(intent("durable/after-panic", "no")),
            Err(DurableSubmitError::Poisoned { .. })
        ));
        assert_eq!(database.snapshot().version(), 0);
        assert_eq!(database.frame_count(), 0);
        let report = controller.shutdown();
        assert_eq!(report.final_state, DurableControllerState::Poisoned);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn lifecycle_metrics_measure_the_complete_epoch_path() {
        let path = temp_path("timings");
        let store =
            FileEpochStore::open(&path, FileEpochSyncPolicy::PerEpoch).expect("epoch store opens");
        let database = Arc::new(Database::new(store).expect("database opens"));
        let controller =
            DurableQueuedIntentController::new(database, 16, 8).expect("controller starts");
        controller
            .submit(intent("durable/timing", "yes"))
            .expect("intent admitted")
            .wait()
            .expect("ticket resolves");
        controller.flush().expect("timing barrier completes");
        let metrics = controller.metrics();
        assert!(metrics.queue_wait_nanos > 0);
        assert!(metrics.derive_nanos > 0);
        assert!(metrics.persist_nanos > 0);
        assert!(metrics.epoch_total_nanos >= metrics.derive_nanos);
        let ceiling = metrics
            .estimated_pipeline_speedup_ceiling()
            .expect("derive and persist timings exist");
        assert!((1.0..=2.0).contains(&ceiling));
        controller.shutdown();
        let _ = fs::remove_file(path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn speculative_io_uring_prepares_a_successor_before_publication() {
        use std::sync::Condvar;
        use std::time::{Duration, Instant};

        let path = temp_path("speculative-io-uring");
        let store = match IoUringEpochFileIo::open_store_with_entries(&path, 64) {
            Ok(store) => store,
            Err(FileEpochStoreError::Io { source, .. })
                if matches!(source.raw_os_error(), Some(1 | 38 | 95)) =>
            {
                let _ = fs::remove_file(path);
                return;
            }
            Err(error) => panic!("io_uring epoch store should open: {error}"),
        };
        let database = Arc::new(Database::new(store).expect("database opens"));
        let entered = Arc::new((std::sync::Mutex::new(false), Condvar::new()));
        let release = Arc::new((std::sync::Mutex::new(false), Condvar::new()));
        let block_first = Arc::new(AtomicBool::new(true));
        let validator_entered = entered.clone();
        let validator_release = release.clone();
        let validator_block_first = block_first.clone();
        database.register_validator(move |_| {
            if validator_block_first.swap(false, Ordering::SeqCst) {
                let (lock, signal) = &*validator_entered;
                *lock.lock().expect("entered lock") = true;
                signal.notify_all();
                let (lock, signal) = &*validator_release;
                let mut released = lock.lock().expect("release lock");
                while !*released {
                    released = signal.wait(released).expect("release wait");
                }
            }
            Ok(())
        });

        let controller = DurableQueuedIntentController::new_speculative(database.clone(), 16, 1)
            .expect("speculative controller starts");
        let first = controller
            .submit(intent("speculative/0", "zero"))
            .expect("first admitted");

        let (lock, signal) = &*entered;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut observed = lock.lock().expect("entered lock");
        while !*observed {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "validator was never entered");
            let (next, timeout) = signal
                .wait_timeout(observed, remaining)
                .expect("entered wait");
            observed = next;
            assert!(
                !timeout.timed_out() || *observed,
                "validator entry timed out"
            );
        }
        drop(observed);

        let second = controller
            .submit(intent("speculative/1", "one"))
            .expect("second admitted");
        let third = controller
            .submit(intent("speculative/2", "two"))
            .expect("third admitted");
        let (lock, signal) = &*release;
        *lock.lock().expect("release lock") = true;
        signal.notify_all();

        for ticket in [first, second, third] {
            assert!(matches!(
                ticket.wait().expect("ticket resolves"),
                DurableTicketOutcome::Accepted { .. }
            ));
        }
        controller.flush().expect("pipeline drains");
        let metrics = controller.metrics();
        assert!(metrics.speculative_epochs_prepared >= 1);
        assert_eq!(metrics.speculative_epochs_rederived, 0);
        assert_eq!(database.snapshot().version(), 3);
        assert_eq!(database.frame_count(), 3);
        assert_eq!(controller.store_metrics().data_syncs, 3);
        controller.shutdown();
        drop(controller);
        drop(database);
        let reopened = FileCommitStore::open(&path).expect("history reopens");
        assert_eq!(reopened.len(), 3);
        let _ = fs::remove_file(path);
    }
}
