use super::*;
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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

#[derive(Debug)]
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

impl fmt::Display for TicketRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorldPrecondition { expected, actual } => write!(
                f,
                "world precondition mismatch: expected {expected:?}, actual {actual:?}"
            ),
            Self::SlotPrecondition {
                slot,
                expected,
                actual,
            } => write!(
                f,
                "slot precondition mismatch on {slot:?}: expected {expected:?}, actual {actual:?}"
            ),
            Self::UnknownTemporaryEntity(temp) => write!(f, "unknown temporary entity: {temp:?}"),
            Self::Candidate(msg) => write!(f, "candidate rejection: {msg}"),
            Self::Validation(msg) => write!(f, "validation failure: {msg}"),
        }
    }
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
            IntentRejection::UnknownTemporaryEntity(temp) => Self::UnknownTemporaryEntity(*temp),
            IntentRejection::Candidate(message) => Self::Candidate(message.to_string()),
            IntentRejection::Validation(message) => Self::Validation(message.clone()),
        }
    }
}

#[derive(Debug)]
pub enum TicketOutcome {
    Accepted {
        world: Arc<World>,
        entities: BTreeMap<TempEntity, EntityId>,
    },
    Rejected(TicketRejection),
}

impl TicketOutcome {
    fn from_epoch_outcome(outcome: &EpochOutcome) -> Self {
        match outcome {
            EpochOutcome::Accepted(accepted) => Self::Accepted {
                world: accepted.world().clone(),
                entities: accepted.entities().clone(),
            },
            EpochOutcome::Rejected(rejected) => {
                Self::Rejected(TicketRejection::from_intent_rejection(rejected.error()))
            }
        }
    }
}

#[derive(Debug)]
pub struct CommitTicket {
    id: u64,
    receiver: Option<mpsc::Receiver<TicketOutcome>>,
    lifecycle: Arc<TicketLifecycle>,
    metrics: Arc<ControllerMetricsInner>,
    observed: bool,
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
            .expect("wait can be called at most once");
        match receiver.recv() {
            Ok(outcome) => {
                self.observed = true;
                Ok(outcome)
            }
            Err(_) => Err(TicketWaitError::WorkerStopped),
        }
    }
}

impl Drop for CommitTicket {
    fn drop(&mut self) {
        if !self.observed {
            if self.lifecycle.abandon() {
                self.metrics
                    .abandoned_tickets
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TicketWaitError {
    WorkerStopped,
}

impl fmt::Display for TicketWaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkerStopped => write!(formatter, "queued intent worker stopped"),
        }
    }
}

impl Error for TicketWaitError {}

#[derive(Debug)]
pub enum SubmitError {
    Full(QueuedIntent),
    Closed(QueuedIntent),
}

impl fmt::Display for SubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => write!(formatter, "queued intent controller channel full"),
            Self::Closed(_) => write!(formatter, "queued intent controller worker stopped"),
        }
    }
}

impl Error for SubmitError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControllerConfigError {
    ZeroCapacity,
    ZeroBatchSize,
    Spawn(std::io::ErrorKind),
}

impl fmt::Display for ControllerConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCapacity => write!(formatter, "capacity must be positive"),
            Self::ZeroBatchSize => write!(formatter, "max batch must be positive"),
            Self::Spawn(kind) => write!(formatter, "worker thread spawn failed: {kind:?}"),
        }
    }
}

impl Error for ControllerConfigError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControllerStopped;

impl fmt::Display for ControllerStopped {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "queued intent controller worker stopped")
    }
}

impl Error for ControllerStopped {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchPolicy {
    ImmediateDrain {
        max_batch: usize,
    },
    Coalesce {
        max_batch: usize,
        max_delay: Duration,
    },
    Adaptive {
        min_batch: usize,
        max_batch: usize,
        latency_budget: Duration,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BatchSealReason {
    Capacity,
    Timeout,
    Drain,
    Width,
    Latency,
    LowTraffic,
    SourceStalled,
    Barrier,
}

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

#[derive(Debug, Default)]
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

    fn record_oldest_age_at_seal(&self, age: Duration) {
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

struct StagedIntent {
    intent: QueuedIntent,
    completion: mpsc::Sender<TicketOutcome>,
    lifecycle: Arc<TicketLifecycle>,
    enqueued_at: Instant,
}

enum ControllerCommand {
    Intent(StagedIntent),
    Barrier(mpsc::Sender<()>),
}

const ARRIVAL_ALPHA: f64 = 0.20;

pub(crate) struct AdaptiveState {
    pub(crate) ewma_interarrival_time_ns: f64,
    pub(crate) last_arrival_time: Option<Instant>,
    pub(crate) target_width: usize,
    pub(crate) epochs_sealed: u64,
}

impl AdaptiveState {
    pub(crate) fn new(initial_target: usize) -> Self {
        Self {
            ewma_interarrival_time_ns: 10_000.0,
            last_arrival_time: None,
            target_width: initial_target,
            epochs_sealed: 0,
        }
    }

    pub(crate) fn observe_arrival(&mut self, enqueued_at: Instant) {
        if let Some(previous) = self.last_arrival_time {
            if let Some(interval) = enqueued_at.checked_duration_since(previous) {
                let interval_ns = interval.as_nanos() as f64;
                self.ewma_interarrival_time_ns = (1.0 - ARRIVAL_ALPHA)
                    * self.ewma_interarrival_time_ns
                    + ARRIVAL_ALPHA * interval_ns;
            }
        }
        self.last_arrival_time = Some(
            self.last_arrival_time
                .map_or(enqueued_at, |previous| previous.max(enqueued_at)),
        );
    }

    pub(crate) fn update_target(
        &mut self,
        reason: BatchSealReason,
        achieved_width: usize,
        min_batch: usize,
        max_batch: usize,
    ) {
        self.epochs_sealed += 1;
        match reason {
            BatchSealReason::Width if achieved_width >= self.target_width => {
                self.target_width = (self.target_width + 1).saturating_mul(2).min(max_batch);
            }
            BatchSealReason::Width => {
                self.target_width = self
                    .target_width
                    .saturating_add(min_batch.max(1))
                    .min(max_batch);
            }
            BatchSealReason::Latency => {
                self.target_width = self.target_width.saturating_div(2).max(min_batch);
            }
            BatchSealReason::SourceStalled => {
                self.target_width = achieved_width.max(min_batch).min(max_batch);
            }
            BatchSealReason::LowTraffic => {
                self.target_width = achieved_width.max(min_batch).min(max_batch);
            }
            _ => {}
        }
    }
}

pub struct QueuedIntentController {
    database: Arc<Database<MemoryCommitStore>>,
    sender: Option<SyncSender<ControllerCommand>>,
    worker: Option<JoinHandle<()>>,
    metrics: Arc<ControllerMetricsInner>,
    capacity: usize,
    policy: BatchPolicy,
}

impl fmt::Debug for QueuedIntentController {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueuedIntentController")
            .field("capacity", &self.capacity)
            .field("policy", &self.policy)
            .field("metrics", &self.metrics())
            .finish()
    }
}

impl QueuedIntentController {
    pub fn new(
        database: Arc<Database<MemoryCommitStore>>,
        capacity: usize,
        policy: BatchPolicy,
    ) -> Result<Self, ControllerConfigError> {
        if capacity == 0 {
            return Err(ControllerConfigError::ZeroCapacity);
        }
        let max_batch = match policy {
            BatchPolicy::ImmediateDrain { max_batch }
            | BatchPolicy::Coalesce { max_batch, .. }
            | BatchPolicy::Adaptive { max_batch, .. } => max_batch,
        };
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
            .spawn(move || run_worker(worker_database, receiver, policy, worker_metrics))
            .map_err(|error| {
                metrics.worker_alive.store(false, Ordering::Release);
                ControllerConfigError::Spawn(error.kind())
            })?;

        Ok(Self {
            database,
            sender: Some(sender),
            worker: Some(worker),
            metrics,
            capacity,
            policy,
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
            enqueued_at: Instant::now(),
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
        let max_batch = match self.policy {
            BatchPolicy::ImmediateDrain { max_batch }
            | BatchPolicy::Coalesce { max_batch, .. }
            | BatchPolicy::Adaptive { max_batch, .. } => max_batch,
        };
        self.metrics.snapshot(self.capacity, max_batch)
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
    policy: BatchPolicy,
    metrics: Arc<ControllerMetricsInner>,
) {
    let _liveness = WorkerLiveness(metrics.clone());
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

        match command {
            ControllerCommand::Intent(first) => {
                let mut batch = Vec::with_capacity(max_batch);
                let mut oldest_enqueued_at = first.enqueued_at;
                let enqueued_at = first.enqueued_at;
                claim(first, &metrics, &mut batch);
                state.observe_arrival(enqueued_at);

                let seal_reason = match policy {
                    BatchPolicy::ImmediateDrain { .. } => {
                        while batch.len() < max_batch {
                            match receiver.try_recv() {
                                Ok(ControllerCommand::Intent(staged)) => {
                                    let enqueued_at = staged.enqueued_at;
                                    oldest_enqueued_at = oldest_enqueued_at.min(enqueued_at);
                                    claim(staged, &metrics, &mut batch);
                                    state.observe_arrival(enqueued_at);
                                }
                                Ok(command @ ControllerCommand::Barrier(_)) => {
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
                        while batch.len() < max_batch {
                            let now = Instant::now();
                            if now >= deadline {
                                break;
                            }
                            let remaining = deadline - now;
                            match receiver.recv_timeout(remaining) {
                                Ok(ControllerCommand::Intent(staged)) => {
                                    let enqueued_at = staged.enqueued_at;
                                    oldest_enqueued_at = oldest_enqueued_at.min(enqueued_at);
                                    claim(staged, &metrics, &mut batch);
                                    state.observe_arrival(enqueued_at);
                                }
                                Ok(command @ ControllerCommand::Barrier(_)) => {
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
                        let batch_start = Instant::now();

                        // 1. Immediate drain phase
                        while batch.len() < state.target_width {
                            match receiver.try_recv() {
                                Ok(ControllerCommand::Intent(staged)) => {
                                    let enqueued_at = staged.enqueued_at;
                                    oldest_enqueued_at = oldest_enqueued_at.min(enqueued_at);
                                    claim(staged, &metrics, &mut batch);
                                    state.observe_arrival(enqueued_at);
                                }
                                Ok(command @ ControllerCommand::Barrier(_)) => {
                                    pending.push_back(command);
                                    break;
                                }
                                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                            }
                        }

                        let mut reason = if batch.len() >= state.target_width {
                            BatchSealReason::Width
                        } else {
                            // 2. Predictive probe wait phase
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
                                    Ok(ControllerCommand::Intent(staged)) => {
                                        let enqueued_at = staged.enqueued_at;
                                        oldest_enqueued_at = oldest_enqueued_at.min(enqueued_at);
                                        claim(staged, &metrics, &mut batch);
                                        state.observe_arrival(enqueued_at);

                                        while batch.len() < state.target_width {
                                            match receiver.try_recv() {
                                                Ok(ControllerCommand::Intent(staged)) => {
                                                    let enqueued_at = staged.enqueued_at;
                                                    oldest_enqueued_at =
                                                        oldest_enqueued_at.min(enqueued_at);
                                                    claim(staged, &metrics, &mut batch);
                                                    state.observe_arrival(enqueued_at);
                                                }
                                                Ok(command @ ControllerCommand::Barrier(_)) => {
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
                                    Ok(command @ ControllerCommand::Barrier(_)) => {
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
                process_batch(&database, batch, &metrics);
            }
            ControllerCommand::Barrier(completed) => {
                metrics.record_seal_reason(BatchSealReason::Barrier);
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

        let controller =
            QueuedIntentController::new(database, 1, BatchPolicy::ImmediateDrain { max_batch: 1 })
                .expect("controller starts");
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

        let controller = QueuedIntentController::new(
            database.clone(),
            4,
            BatchPolicy::ImmediateDrain { max_batch: 4 },
        )
        .expect("controller starts");
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
        let controller = QueuedIntentController::new(
            database.clone(),
            8,
            BatchPolicy::ImmediateDrain { max_batch: 8 },
        )
        .expect("controller starts");
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
        let controller = QueuedIntentController::new(
            database.clone(),
            4,
            BatchPolicy::ImmediateDrain { max_batch: 4 },
        )
        .expect("controller starts");
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
        let controller =
            QueuedIntentController::new(database, 4, BatchPolicy::ImmediateDrain { max_batch: 4 })
                .expect("controller starts");

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
