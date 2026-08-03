use super::*;
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

const PHASE_QUEUED: u8 = 0;
const PHASE_CLAIMED: u8 = 1;
const PHASE_RESOLVED: u8 = 2;

static NEXT_TICKET_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TicketPhase {
    Queued,
    Claimed,
    Resolved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TicketState {
    pub phase: TicketPhase,
    pub abandoned: bool,
}

struct TicketLifecycle {
    phase: AtomicU8,
    abandoned: AtomicBool,
}

impl TicketLifecycle {
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

    fn snapshot(&self) -> TicketState {
        let phase = match self.phase.load(Ordering::Acquire) {
            PHASE_QUEUED => TicketPhase::Queued,
            PHASE_CLAIMED => TicketPhase::Claimed,
            PHASE_RESOLVED => TicketPhase::Resolved,
            _ => unreachable!("invalid commit-ticket phase"),
        };
        TicketState {
            phase,
            abandoned: self.abandoned.load(Ordering::Acquire),
        }
    }
}

#[derive(Debug)]
pub enum TicketRejection {
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

impl TicketRejection {
    fn from_intent_rejection(error: &IntentRejection) -> Self {
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

impl fmt::Display for TicketRejection {
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

impl Error for TicketRejection {}

#[derive(Debug)]
pub enum TicketOutcome {
    Accepted {
        world: Arc<World>,
        frame: Arc<CommitFrame>,
        entities: BTreeMap<TempEntity, EntityId>,
    },
    Rejected(TicketRejection),
}

impl TicketOutcome {
    fn from_epoch_outcome(outcome: &EpochOutcome) -> Self {
        match outcome {
            EpochOutcome::Accepted(accepted) => Self::Accepted {
                world: accepted.world(),
                frame: accepted.frame(),
                entities: accepted.entities().clone(),
            },
            EpochOutcome::Rejected(rejected) => {
                Self::Rejected(TicketRejection::from_intent_rejection(rejected.error()))
            }
        }
    }

    pub fn world(&self) -> Option<Arc<World>> {
        match self {
            Self::Accepted { world, .. } => Some(world.clone()),
            Self::Rejected(_) => None,
        }
    }

    pub fn frame(&self) -> Option<Arc<CommitFrame>> {
        match self {
            Self::Accepted { frame, .. } => Some(frame.clone()),
            Self::Rejected(_) => None,
        }
    }

    pub fn entity(&self, temporary: TempEntity) -> Option<EntityId> {
        match self {
            Self::Accepted { entities, .. } => entities.get(&temporary).copied(),
            Self::Rejected(_) => None,
        }
    }

    pub fn rejection(&self) -> Option<&TicketRejection> {
        match self {
            Self::Accepted { .. } => None,
            Self::Rejected(error) => Some(error),
        }
    }
}

#[derive(Debug)]
pub struct TicketWaitError;

impl fmt::Display for TicketWaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "queued-intent controller stopped before resolving ticket"
        )
    }
}

impl Error for TicketWaitError {}

pub struct CommitTicket {
    id: u64,
    receiver: Option<Receiver<TicketOutcome>>,
    lifecycle: Arc<TicketLifecycle>,
    metrics: Arc<ControllerMetricsInner>,
    observed: bool,
}

impl fmt::Debug for CommitTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommitTicket")
            .field("id", &self.id)
            .field("state", &self.state())
            .finish()
    }
}

impl CommitTicket {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn state(&self) -> TicketState {
        self.lifecycle.snapshot()
    }

    pub fn wait(mut self) -> Result<TicketOutcome, TicketWaitError> {
        let receiver = self
            .receiver
            .take()
            .expect("commit ticket receiver can be consumed only once");
        let result = receiver.recv().map_err(|_| TicketWaitError);
        self.observed = true;
        result
    }

    pub fn try_wait(&mut self) -> Result<Option<TicketOutcome>, TicketWaitError> {
        let receiver = self
            .receiver
            .as_ref()
            .expect("commit ticket receiver can be consumed only once");
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
                Err(TicketWaitError)
            }
        }
    }
}

impl Drop for CommitTicket {
    fn drop(&mut self) {
        if !self.observed && self.lifecycle.abandon() {
            self.metrics
                .abandoned_tickets
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Debug)]
pub enum SubmitError {
    Full(QueuedIntent),
    Closed(QueuedIntent),
}

impl SubmitError {
    pub fn into_intent(self) -> QueuedIntent {
        match self {
            Self::Full(intent) | Self::Closed(intent) => intent,
        }
    }
}

impl fmt::Display for SubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => write!(formatter, "queued-intent ingress is full"),
            Self::Closed(_) => write!(formatter, "queued-intent controller is closed"),
        }
    }
}

impl Error for SubmitError {}

#[derive(Debug)]
pub enum ControllerConfigError {
    ZeroCapacity,
    ZeroBatchSize,
    Spawn(std::io::Error),
}

impl fmt::Display for ControllerConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCapacity => write!(formatter, "queued ingress capacity must be nonzero"),
            Self::ZeroBatchSize => write!(formatter, "queued maximum batch size must be nonzero"),
            Self::Spawn(error) => write!(formatter, "failed to start queued committer: {error}"),
        }
    }
}

impl Error for ControllerConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            Self::ZeroCapacity | Self::ZeroBatchSize => None,
        }
    }
}

#[derive(Debug)]
pub struct ControllerStopped;

impl fmt::Display for ControllerStopped {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "queued-intent controller is stopped")
    }
}

impl Error for ControllerStopped {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueuedControllerMetrics {
    pub capacity: usize,
    pub max_batch: usize,
    pub submitted: u64,
    pub backpressured: u64,
    pub claimed: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub epochs: u64,
    pub abandoned_tickets: u64,
    pub completion_delivery_failures: u64,
    pub queue_depth: u64,
    pub maximum_queue_depth: u64,
    pub in_flight: u64,
    pub worker_alive: bool,
}

#[derive(Default)]
struct ControllerMetricsInner {
    submitted: AtomicU64,
    backpressured: AtomicU64,
    claimed: AtomicU64,
    accepted: AtomicU64,
    rejected: AtomicU64,
    epochs: AtomicU64,
    abandoned_tickets: AtomicU64,
    completion_delivery_failures: AtomicU64,
    queue_depth: AtomicU64,
    maximum_queue_depth: AtomicU64,
    in_flight: AtomicU64,
    worker_alive: AtomicBool,
}

impl ControllerMetricsInner {
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
        debug_assert!(previous > 0, "ingress reservation counter underflow");
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

    fn snapshot(&self, capacity: usize, max_batch: usize) -> QueuedControllerMetrics {
        QueuedControllerMetrics {
            capacity,
            max_batch,
            submitted: self.submitted.load(Ordering::Relaxed),
            backpressured: self.backpressured.load(Ordering::Relaxed),
            claimed: self.claimed.load(Ordering::Relaxed),
            accepted: self.accepted.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            epochs: self.epochs.load(Ordering::Relaxed),
            abandoned_tickets: self.abandoned_tickets.load(Ordering::Relaxed),
            completion_delivery_failures: self.completion_delivery_failures.load(Ordering::Relaxed),
            queue_depth: self.queue_depth.load(Ordering::Acquire),
            maximum_queue_depth: self.maximum_queue_depth.load(Ordering::Relaxed),
            in_flight: self.in_flight.load(Ordering::Acquire),
            worker_alive: self.worker_alive.load(Ordering::Acquire),
        }
    }
}

struct StagedIntent {
    intent: QueuedIntent,
    completion: mpsc::Sender<TicketOutcome>,
    lifecycle: Arc<TicketLifecycle>,
}

enum ControllerCommand {
    Intent(StagedIntent),
    Barrier(mpsc::Sender<()>),
}

/// Bounded Stage 6A.2 ingress and in-memory committer.
///
/// The worker blocks for the first intent, drains an already-arrived burst up
/// to `max_batch`, derives and publishes one in-memory epoch, then resolves each
/// caller independently. It performs no file I/O and makes no durability claim.
pub struct QueuedIntentController {
    database: Arc<Database<MemoryCommitStore>>,
    sender: Option<SyncSender<ControllerCommand>>,
    worker: Option<JoinHandle<()>>,
    metrics: Arc<ControllerMetricsInner>,
    capacity: usize,
    max_batch: usize,
}

impl fmt::Debug for QueuedIntentController {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueuedIntentController")
            .field("capacity", &self.capacity)
            .field("max_batch", &self.max_batch)
            .field("metrics", &self.metrics())
            .finish()
    }
}

impl QueuedIntentController {
    pub fn new(
        database: Arc<Database<MemoryCommitStore>>,
        capacity: usize,
        max_batch: usize,
    ) -> Result<Self, ControllerConfigError> {
        if capacity == 0 {
            return Err(ControllerConfigError::ZeroCapacity);
        }
        if max_batch == 0 {
            return Err(ControllerConfigError::ZeroBatchSize);
        }

        let (sender, receiver) = mpsc::sync_channel(capacity);
        let metrics = Arc::new(ControllerMetricsInner::default());
        metrics.worker_alive.store(true, Ordering::Release);
        let worker_database = database.clone();
        let worker_metrics = metrics.clone();
        let worker = thread::Builder::new()
            .name("forthdb-queued-committer".to_owned())
            .spawn(move || run_worker(worker_database, receiver, max_batch, worker_metrics))
            .map_err(|error| {
                metrics.worker_alive.store(false, Ordering::Release);
                ControllerConfigError::Spawn(error)
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

    pub fn database(&self) -> Arc<Database<MemoryCommitStore>> {
        self.database.clone()
    }

    pub fn submit(&self, intent: QueuedIntent) -> Result<CommitTicket, SubmitError> {
        let Some(sender) = self.sender.as_ref() else {
            return Err(SubmitError::Closed(intent));
        };
        let Some(depth) = self.metrics.reserve_ingress(self.capacity) else {
            self.metrics.backpressured.fetch_add(1, Ordering::Relaxed);
            return Err(SubmitError::Full(intent));
        };

        let lifecycle = Arc::new(TicketLifecycle::new());
        let (completion, receiver) = mpsc::channel();
        let command = ControllerCommand::Intent(StagedIntent {
            intent,
            completion,
            lifecycle: lifecycle.clone(),
        });

        match sender.try_send(command) {
            Ok(()) => {
                self.metrics.observe_queue_depth(depth);
                self.metrics.submitted.fetch_add(1, Ordering::Relaxed);
                Ok(CommitTicket {
                    id: NEXT_TICKET_ID.fetch_add(1, Ordering::Relaxed),
                    receiver: Some(receiver),
                    lifecycle,
                    metrics: self.metrics.clone(),
                    observed: false,
                })
            }
            Err(TrySendError::Full(ControllerCommand::Intent(staged))) => {
                self.metrics.release_ingress();
                self.metrics.backpressured.fetch_add(1, Ordering::Relaxed);
                Err(SubmitError::Full(staged.intent))
            }
            Err(TrySendError::Disconnected(ControllerCommand::Intent(staged))) => {
                self.metrics.release_ingress();
                Err(SubmitError::Closed(staged.intent))
            }
            Err(TrySendError::Full(ControllerCommand::Barrier(_)))
            | Err(TrySendError::Disconnected(ControllerCommand::Barrier(_))) => unreachable!(),
        }
    }

    /// Wait until every command submitted before this call has been processed.
    /// This is an administrative/test barrier, not a durability operation.
    pub fn flush(&self) -> Result<(), ControllerStopped> {
        let Some(sender) = self.sender.as_ref() else {
            return Err(ControllerStopped);
        };
        let (completed, receiver) = mpsc::channel();
        sender
            .send(ControllerCommand::Barrier(completed))
            .map_err(|_| ControllerStopped)?;
        receiver.recv().map_err(|_| ControllerStopped)
    }

    pub fn metrics(&self) -> QueuedControllerMetrics {
        self.metrics.snapshot(self.capacity, self.max_batch)
    }
}

impl Drop for QueuedIntentController {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct WorkerLiveness(Arc<ControllerMetricsInner>);

impl Drop for WorkerLiveness {
    fn drop(&mut self) {
        self.0.worker_alive.store(false, Ordering::Release);
    }
}

fn run_worker(
    database: Arc<Database<MemoryCommitStore>>,
    receiver: Receiver<ControllerCommand>,
    max_batch: usize,
    metrics: Arc<ControllerMetricsInner>,
) {
    let _liveness = WorkerLiveness(metrics.clone());
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
            ControllerCommand::Intent(first) => {
                let mut batch = Vec::with_capacity(max_batch);
                claim(first, &metrics, &mut batch);
                while batch.len() < max_batch {
                    match receiver.try_recv() {
                        Ok(ControllerCommand::Intent(staged)) => {
                            claim(staged, &metrics, &mut batch);
                        }
                        Ok(command @ ControllerCommand::Barrier(_)) => {
                            pending.push_back(command);
                            break;
                        }
                        Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                    }
                }
                process_batch(&database, batch, &metrics);
            }
            ControllerCommand::Barrier(completed) => {
                let _ = completed.send(());
            }
        }
    }
}

fn claim(staged: StagedIntent, metrics: &ControllerMetricsInner, batch: &mut Vec<StagedIntent>) {
    metrics.release_ingress();
    metrics.claimed.fetch_add(1, Ordering::Relaxed);
    staged.lifecycle.claim();
    batch.push(staged);
}

fn process_batch(
    database: &Database<MemoryCommitStore>,
    batch: Vec<StagedIntent>,
    metrics: &ControllerMetricsInner,
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

    // The memory control returns only after accepted frames are appended and
    // the reader head is swapped once to the epoch tail.
    let plan = database.commit_queued_epoch(intents);
    debug_assert_eq!(plan.outcomes().len(), routes.len());

    for ((completion, lifecycle), outcome) in routes.into_iter().zip(plan.outcomes()) {
        let ticket_outcome = TicketOutcome::from_epoch_outcome(outcome);
        match &ticket_outcome {
            TicketOutcome::Accepted { .. } => {
                metrics.accepted.fetch_add(1, Ordering::Relaxed);
            }
            TicketOutcome::Rejected(_) => {
                metrics.rejected.fetch_add(1, Ordering::Relaxed);
            }
        }
        lifecycle.resolve();
        if completion.send(ticket_outcome).is_err() {
            metrics
                .completion_delivery_failures
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    metrics.epochs.fetch_add(1, Ordering::Relaxed);
    metrics.in_flight.store(0, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn state_fact(entity: EntityId, value: &str) -> Fact {
        Fact::new(
            Atom::Entity(entity),
            Predicate::new("state"),
            Atom::Literal(Literal::new(value)),
        )
    }

    fn accepted_world(outcome: TicketOutcome) -> Arc<World> {
        match outcome {
            TicketOutcome::Accepted { world, .. } => world,
            TicketOutcome::Rejected(error) => panic!("expected acceptance, found {error}"),
        }
    }

    #[test]
    fn saturation_returns_the_original_intent_immediately() {
        let database =
            Arc::new(Database::new(MemoryCommitStore::new()).expect("empty memory store is valid"));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let block_once = Arc::new(AtomicBool::new(true));
        let validator_release = release_rx.clone();
        let validator_once = block_once.clone();
        database.register_validator(move |_| {
            if validator_once.swap(false, Ordering::SeqCst) {
                entered_tx.send(()).expect("test receives validator entry");
                validator_release
                    .lock()
                    .expect("release receiver lock")
                    .recv()
                    .expect("test releases validator");
            }
            Ok(())
        });

        let controller = QueuedIntentController::new(database, 1, 1).expect("controller starts");
        let first = controller
            .submit(QueuedIntent::new())
            .expect("first intent enters worker");
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker reaches validator");
        assert_eq!(first.state().phase, TicketPhase::Claimed);

        let second = controller
            .submit(QueuedIntent::new())
            .expect("second intent fills bounded queue");
        let error = controller
            .submit(QueuedIntent::new())
            .expect_err("third intent must receive immediate backpressure");
        assert!(matches!(error, SubmitError::Full(_)));

        release_tx.send(()).expect("release worker");
        accepted_world(first.wait().expect("first resolves"));
        accepted_world(second.wait().expect("second resolves"));
        controller.flush().expect("controller drains");

        let metrics = controller.metrics();
        assert_eq!(metrics.submitted, 2);
        assert_eq!(metrics.backpressured, 1);
        assert_eq!(metrics.claimed, 2);
        assert_eq!(metrics.accepted, 2);
        assert_eq!(metrics.queue_depth, 0);
        assert!(metrics.maximum_queue_depth <= metrics.capacity as u64);
    }

    #[test]
    fn dropping_a_claimed_ticket_does_not_cancel_history() {
        let database =
            Arc::new(Database::new(MemoryCommitStore::new()).expect("empty memory store is valid"));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let validator_release = release_rx.clone();
        database.register_validator(move |_| {
            entered_tx.send(()).expect("test receives validator entry");
            validator_release
                .lock()
                .expect("release receiver lock")
                .recv()
                .expect("test releases validator");
            Ok(())
        });

        let controller =
            QueuedIntentController::new(database.clone(), 4, 4).expect("controller starts");
        let mut intent = QueuedIntent::new();
        intent.define_fact(
            SlotId::new("abandoned/state"),
            state_fact(EntityId::new(99), "committed"),
        );
        let ticket = controller.submit(intent).expect("intent submitted");
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker reaches validator");
        assert_eq!(ticket.state().phase, TicketPhase::Claimed);
        drop(ticket);
        release_tx.send(()).expect("release worker");
        controller
            .flush()
            .expect("abandoned intent still completes");

        let world = database.snapshot();
        assert_eq!(world.version(), 1);
        assert_eq!(
            world
                .resolve(&SlotId::new("abandoned/state"))
                .map(|fact| fact.object.clone()),
            Some(Atom::Literal(Literal::new("committed")))
        );
        assert_eq!(controller.metrics().abandoned_tickets, 1);
    }

    #[test]
    fn rejection_routes_only_to_its_own_ticket() {
        let database =
            Arc::new(Database::new(MemoryCommitStore::new()).expect("empty memory store is valid"));
        let controller =
            QueuedIntentController::new(database.clone(), 8, 8).expect("controller starts");
        let slot = SlotId::new("pipeline/state");

        let mut first = QueuedIntent::new();
        first.define_fact(slot.clone(), state_fact(EntityId::new(1), "one"));
        let first_world = accepted_world(
            controller
                .submit(first)
                .expect("first submitted")
                .wait()
                .expect("first resolves"),
        );
        assert_eq!(first_world.version(), 1);

        let mut rejected = QueuedIntent::new();
        rejected.expect_absent(slot.clone());
        rejected.define_fact(
            SlotId::new("pipeline/rejected"),
            state_fact(EntityId::new(2), "never"),
        );
        let rejected = controller
            .submit(rejected)
            .expect("rejected intent submitted");

        let mut third = QueuedIntent::new();
        third.define_fact(
            SlotId::new("pipeline/third"),
            state_fact(EntityId::new(3), "three"),
        );
        let third = controller.submit(third).expect("third submitted");

        assert!(matches!(
            rejected.wait().expect("rejection is delivered"),
            TicketOutcome::Rejected(TicketRejection::SlotPrecondition { .. })
        ));
        let third_world = accepted_world(third.wait().expect("third resolves"));
        controller.flush().expect("controller drains");

        assert_eq!(third_world.version(), 2);
        assert!(
            database
                .snapshot()
                .resolve(&SlotId::new("pipeline/rejected"))
                .is_none()
        );
        assert!(
            database
                .snapshot()
                .resolve(&SlotId::new("pipeline/third"))
                .is_some()
        );
        let metrics = controller.metrics();
        assert_eq!(metrics.accepted, 2);
        assert_eq!(metrics.rejected, 1);
    }

    #[test]
    fn ticket_resolution_never_precedes_tail_publication() {
        let database =
            Arc::new(Database::new(MemoryCommitStore::new()).expect("empty memory store is valid"));
        let controller =
            QueuedIntentController::new(database.clone(), 4, 4).expect("controller starts");
        let slot = SlotId::new("published/state");
        let mut intent = QueuedIntent::new();
        intent.define_fact(slot.clone(), state_fact(EntityId::new(7), "visible"));

        let world = accepted_world(
            controller
                .submit(intent)
                .expect("intent submitted")
                .wait()
                .expect("ticket resolves"),
        );
        assert_eq!(database.snapshot().id(), world.id());
        assert!(database.snapshot().resolve(&slot).is_some());
    }

    #[test]
    fn fast_claims_never_underflow_ingress_accounting() {
        let database =
            Arc::new(Database::new(MemoryCommitStore::new()).expect("empty memory store is valid"));
        let controller = QueuedIntentController::new(database, 4, 4).expect("controller starts");

        for _ in 0..1_000 {
            let ticket = loop {
                match controller.submit(QueuedIntent::new()) {
                    Ok(ticket) => break ticket,
                    Err(SubmitError::Full(intent)) => {
                        thread::yield_now();
                        match controller.submit(intent) {
                            Ok(ticket) => break ticket,
                            Err(SubmitError::Full(_)) => continue,
                            Err(SubmitError::Closed(_)) => panic!("controller closed"),
                        }
                    }
                    Err(SubmitError::Closed(_)) => panic!("controller closed"),
                }
            };
            accepted_world(ticket.wait().expect("ticket resolves"));
        }
        controller.flush().expect("controller drains");
        let metrics = controller.metrics();
        assert_eq!(metrics.queue_depth, 0);
        assert!(metrics.maximum_queue_depth <= metrics.capacity as u64);
        assert_eq!(metrics.submitted, 1_000);
        assert_eq!(metrics.claimed, 1_000);
    }
}
