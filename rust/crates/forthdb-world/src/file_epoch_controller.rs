use super::*;
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

const PHASE_QUEUED: u8 = 0;
const PHASE_CLAIMED: u8 = 1;
const PHASE_RESOLVED: u8 = 2;

static NEXT_DURABLE_TICKET_ID: AtomicU64 = AtomicU64::new(1);

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
    fn from_intent(error: &IntentRejection) -> Self {
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

#[derive(Debug)]
pub enum DurableTicketOutcome {
    Accepted {
        world: Arc<World>,
        frame: Arc<CommitFrame>,
        entities: BTreeMap<TempEntity, EntityId>,
    },
    Rejected(DurableTicketRejection),
    DurabilityFailed(String),
}

impl DurableTicketOutcome {
    pub fn world(&self) -> Option<Arc<World>> {
        match self {
            Self::Accepted { world, .. } => Some(world.clone()),
            Self::Rejected(_) | Self::DurabilityFailed(_) => None,
        }
    }

    pub fn rejection(&self) -> Option<&DurableTicketRejection> {
        match self {
            Self::Rejected(error) => Some(error),
            Self::Accepted { .. } | Self::DurabilityFailed(_) => None,
        }
    }

    pub fn durability_error(&self) -> Option<&str> {
        match self {
            Self::DurabilityFailed(message) => Some(message),
            Self::Accepted { .. } | Self::Rejected(_) => None,
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

    pub fn try_wait(
        &mut self,
    ) -> Result<Option<DurableTicketOutcome>, DurableTicketWaitError> {
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
}

impl DurableSubmitError {
    pub fn into_intent(self) -> QueuedIntent {
        match self {
            Self::Full(intent) | Self::Closed(intent) => intent,
        }
    }
}

impl fmt::Display for DurableSubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => write!(formatter, "durable queued-intent ingress is full"),
            Self::Closed(_) => write!(formatter, "durable queued-intent controller is closed"),
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
pub struct DurableControllerStopped;

impl fmt::Display for DurableControllerStopped {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "durable queued-intent controller is stopped")
    }
}

impl Error for DurableControllerStopped {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurableQueuedControllerMetrics {
    pub capacity: usize,
    pub max_batch: usize,
    pub submitted: u64,
    pub backpressured: u64,
    pub claimed: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub durability_failed: u64,
    pub epochs: u64,
    pub abandoned_tickets: u64,
    pub completion_delivery_failures: u64,
    pub queue_depth: u64,
    pub maximum_queue_depth: u64,
    pub in_flight: u64,
    pub worker_alive: bool,
}

#[derive(Default)]
struct DurableControllerMetricsInner {
    submitted: AtomicU64,
    backpressured: AtomicU64,
    claimed: AtomicU64,
    accepted: AtomicU64,
    rejected: AtomicU64,
    durability_failed: AtomicU64,
    epochs: AtomicU64,
    abandoned_tickets: AtomicU64,
    completion_delivery_failures: AtomicU64,
    queue_depth: AtomicU64,
    maximum_queue_depth: AtomicU64,
    in_flight: AtomicU64,
    worker_alive: AtomicBool,
}

impl DurableControllerMetricsInner {
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

    fn snapshot(&self, capacity: usize, max_batch: usize) -> DurableQueuedControllerMetrics {
        DurableQueuedControllerMetrics {
            capacity,
            max_batch,
            submitted: self.submitted.load(Ordering::Relaxed),
            backpressured: self.backpressured.load(Ordering::Relaxed),
            claimed: self.claimed.load(Ordering::Relaxed),
            accepted: self.accepted.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            durability_failed: self.durability_failed.load(Ordering::Relaxed),
            epochs: self.epochs.load(Ordering::Relaxed),
            abandoned_tickets: self.abandoned_tickets.load(Ordering::Relaxed),
            completion_delivery_failures: self
                .completion_delivery_failures
                .load(Ordering::Relaxed),
            queue_depth: self.queue_depth.load(Ordering::Acquire),
            maximum_queue_depth: self.maximum_queue_depth.load(Ordering::Relaxed),
            in_flight: self.in_flight.load(Ordering::Acquire),
            worker_alive: self.worker_alive.load(Ordering::Acquire),
        }
    }
}

struct DurableStagedIntent {
    intent: QueuedIntent,
    completion: mpsc::Sender<DurableTicketOutcome>,
    lifecycle: Arc<DurableTicketLifecycle>,
}

enum DurableControllerCommand {
    Intent(DurableStagedIntent),
    Barrier(mpsc::Sender<()>),
}

pub struct DurableQueuedIntentController<I: EpochFileIo = StdEpochFileIo> {
    database: Arc<Database<FileEpochStore<I>>>,
    sender: Option<SyncSender<DurableControllerCommand>>,
    worker: Option<JoinHandle<()>>,
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

impl<I: EpochFileIo + 'static> DurableQueuedIntentController<I> {
    pub fn new(
        database: Arc<Database<FileEpochStore<I>>>,
        capacity: usize,
        max_batch: usize,
    ) -> Result<Self, DurableControllerConfigError> {
        if capacity == 0 {
            return Err(DurableControllerConfigError::ZeroCapacity);
        }
        if max_batch == 0 {
            return Err(DurableControllerConfigError::ZeroBatchSize);
        }
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let metrics = Arc::new(DurableControllerMetricsInner::default());
        metrics.worker_alive.store(true, Ordering::Release);
        let worker_database = database.clone();
        let worker_metrics = metrics.clone();
        let worker = thread::Builder::new()
            .name("forthdb-file-epoch-committer".to_owned())
            .spawn(move || {
                run_durable_worker(worker_database, receiver, max_batch, worker_metrics)
            })
            .map_err(|error| {
                metrics.worker_alive.store(false, Ordering::Release);
                DurableControllerConfigError::Spawn(error)
            })?;
        Ok(Self {
            database,
            sender: Some(sender),
            worker: Some(worker),
            metrics,
            capacity,
            max_batch,
        })
    }

    pub fn database(&self) -> Arc<Database<FileEpochStore<I>>> {
        self.database.clone()
    }

    pub fn submit(
        &self,
        intent: QueuedIntent,
    ) -> Result<DurableCommitTicket, DurableSubmitError> {
        let Some(sender) = self.sender.as_ref() else {
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
                Err(DurableSubmitError::Closed(staged.intent))
            }
            Err(TrySendError::Full(DurableControllerCommand::Barrier(_)))
            | Err(TrySendError::Disconnected(DurableControllerCommand::Barrier(_))) => {
                unreachable!()
            }
        }
    }

    pub fn flush(&self) -> Result<(), DurableControllerStopped> {
        let Some(sender) = self.sender.as_ref() else {
            return Err(DurableControllerStopped);
        };
        let (completed, receiver) = mpsc::channel();
        sender
            .send(DurableControllerCommand::Barrier(completed))
            .map_err(|_| DurableControllerStopped)?;
        receiver.recv().map_err(|_| DurableControllerStopped)
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

impl<I: EpochFileIo> Drop for DurableQueuedIntentController<I> {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct DurableWorkerLiveness(Arc<DurableControllerMetricsInner>);

impl Drop for DurableWorkerLiveness {
    fn drop(&mut self) {
        self.0.worker_alive.store(false, Ordering::Release);
    }
}

fn run_durable_worker<I: EpochFileIo + 'static>(
    database: Arc<Database<FileEpochStore<I>>>,
    receiver: Receiver<DurableControllerCommand>,
    max_batch: usize,
    metrics: Arc<DurableControllerMetricsInner>,
) {
    let _liveness = DurableWorkerLiveness(metrics.clone());
    let mut pending = VecDeque::new();
    loop {
        let command = match pending.pop_front() {
            Some(command) => command,
            None => match receiver.recv() {
                Ok(command) => command,
                Err(_) => break,
            },
        };
        match command {
            DurableControllerCommand::Intent(first) => {
                let mut batch = Vec::with_capacity(max_batch);
                claim(first, &metrics, &mut batch);
                while batch.len() < max_batch {
                    match receiver.try_recv() {
                        Ok(DurableControllerCommand::Intent(staged)) => {
                            claim(staged, &metrics, &mut batch)
                        }
                        Ok(command @ DurableControllerCommand::Barrier(_)) => {
                            pending.push_back(command);
                            break;
                        }
                        Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                    }
                }
                process_durable_batch(&database, batch, &metrics);
            }
            DurableControllerCommand::Barrier(completed) => {
                let _ = completed.send(());
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
) -> Result<EpochPlan, DurableEpochFailure> {
    let _commit_guard = database
        .commit_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let base = database.snapshot();
    let validators = database
        .validators
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let plan = derive_epoch(base, intents, &validators);
    if plan.is_empty() {
        return Ok(plan);
    }
    let append_result = database
        .store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .append_epoch(plan.frames());
    if let Err(error) = append_result {
        return Err(DurableEpochFailure { plan, error });
    }
    *database
        .current
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = plan.tail();
    Ok(plan)
}

fn process_durable_batch<I: EpochFileIo>(
    database: &Database<FileEpochStore<I>>,
    batch: Vec<DurableStagedIntent>,
    metrics: &DurableControllerMetricsInner,
) {
    metrics
        .in_flight
        .store(batch.len() as u64, Ordering::Release);
    let mut intents = Vec::with_capacity(batch.len());
    let mut routes = Vec::with_capacity(batch.len());
    for staged in batch {
        intents.push(staged.intent);
        routes.push((staged.completion, staged.lifecycle));
    }

    match commit_durable_epoch(database, intents) {
        Ok(plan) => route_success(routes, &plan, metrics),
        Err(failure) => route_failure(routes, &failure.plan, &failure.error, metrics),
    }
    metrics.epochs.fetch_add(1, Ordering::Relaxed);
    metrics.in_flight.store(0, Ordering::Release);
}

fn route_success(
    routes: Vec<(mpsc::Sender<DurableTicketOutcome>, Arc<DurableTicketLifecycle>)>,
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
    routes: Vec<(mpsc::Sender<DurableTicketOutcome>, Arc<DurableTicketLifecycle>)>,
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
        let store = FileEpochStore::open(&path, FileEpochSyncPolicy::PerEpoch)
            .expect("epoch store opens");
        let database = Arc::new(Database::new(store).expect("database opens"));
        let controller = DurableQueuedIntentController::new(database.clone(), 16, 8)
            .expect("controller starts");
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
        let controller = DurableQueuedIntentController::new(database.clone(), 16, 8)
            .expect("controller starts");

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
        let store = FileEpochStore::open(&path, FileEpochSyncPolicy::PerEpoch)
            .expect("epoch store opens");
        let database = Arc::new(Database::new(store).expect("database opens"));
        let controller = DurableQueuedIntentController::new(database.clone(), 16, 8)
            .expect("controller starts");

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
}
