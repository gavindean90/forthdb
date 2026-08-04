use super::*;
#[cfg(target_os = "linux")]
use crate::io_uring_epoch_io::PendingIoUringEpoch;
use crate::queued::{VmEpochMaterializer, decode_queued_intent, encode_queued_intent};
use crate::mmap_vm_snapshot::{MmapSnapshotMetadata, MmapVmSnapshot};
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::fs::OpenOptions;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};

const JOURNAL_MAGIC: &[u8; 8] = b"FTHADM01";
const JOURNAL_VERSION: u32 = 1;
const JOURNAL_HEADER_LEN: usize = 16;
const EPOCH_MAGIC: &[u8; 4] = b"AEP1";
const EPOCH_TRAILER: &[u8; 4] = b"END1";
const EPOCH_PREFIX_LEN: usize = 20;
const MAX_EPOCH_BYTES: usize = 64 * 1024 * 1024;
const CHECKSUM_OFFSET: u64 = 0xcbf29ce484222325;
const CHECKSUM_PRIME: u64 = 0x100000001b3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionEpochReceipt {
    pub epoch_id: u64,
    pub position: usize,
}

#[derive(Debug)]
pub enum AdmissionEpochTicketOutcome {
    Accepted {
        receipt: AdmissionEpochReceipt,
        world: Arc<World>,
        entities: BTreeMap<TempEntity, EntityId>,
    },
    Rejected {
        receipt: AdmissionEpochReceipt,
        error: DurableTicketRejection,
    },
    Failed(String),
}

pub struct AdmissionEpochTicket {
    admission: Receiver<Result<AdmissionEpochReceipt, String>>,
    outcome: Receiver<AdmissionEpochTicketOutcome>,
}

impl AdmissionEpochTicket {
    pub fn wait_admitted(&self) -> Result<AdmissionEpochReceipt, String> {
        self.admission
            .recv()
            .map_err(|_| "admission worker stopped before durability".to_owned())?
    }

    pub fn wait(self) -> Result<AdmissionEpochTicketOutcome, String> {
        self.outcome
            .recv()
            .map_err(|_| "semantic materializer stopped before an outcome".to_owned())
    }
}

#[derive(Debug)]
pub enum AdmissionEpochSubmitError {
    Full(QueuedIntent),
    Closed(QueuedIntent),
}

#[derive(Debug)]
pub enum AdmissionEpochBatchSubmitError {
    Full(Vec<QueuedIntent>),
    Closed(Vec<QueuedIntent>),
}

#[derive(Debug)]
pub enum AdmissionEpochOpenError {
    Io(std::io::Error),
    Format(String),
    Config(String),
    Writer(WriterLeaseError),
}

impl fmt::Display for AdmissionEpochOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "admission journal I/O failed: {error}"),
            Self::Format(error) => write!(formatter, "admission journal is invalid: {error}"),
            Self::Config(error) => write!(
                formatter,
                "admission controller configuration failed: {error}"
            ),
            Self::Writer(error) => write!(formatter, "admission writer lease failed: {error}"),
        }
    }
}

impl Error for AdmissionEpochOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Writer(error) => Some(error),
            Self::Format(_) | Self::Config(_) => None,
        }
    }
}

impl From<std::io::Error> for AdmissionEpochOpenError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<WriterLeaseError> for AdmissionEpochOpenError {
    fn from(value: WriterLeaseError) -> Self {
        Self::Writer(value)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AdmissionEpochMetrics {
    pub submitted_intents: u64,
    pub backpressured_intents: u64,
    pub durable_epochs: u64,
    pub applied_epochs: u64,
    pub published_worlds: u64,
    pub accepted_intents: u64,
    pub rejected_intents: u64,
    pub admitted_bytes: u64,
    pub data_writes: u64,
    pub data_syncs: u64,
    pub completion_events: u64,
    pub maximum_semantic_lag: u64,
    pub vm_materialized_epochs: u64,
    pub world_materialized_epochs: u64,
    pub mmap_snapshot_loaded: bool,
    pub mmap_snapshot_epochs_skipped: u64,
    pub mmap_snapshot_bytes: u64,
}

#[derive(Default)]
struct Metrics {
    submitted_intents: AtomicU64,
    backpressured_intents: AtomicU64,
    durable_epochs: AtomicU64,
    applied_epochs: AtomicU64,
    published_worlds: AtomicU64,
    accepted_intents: AtomicU64,
    rejected_intents: AtomicU64,
    admitted_bytes: AtomicU64,
    data_writes: AtomicU64,
    data_syncs: AtomicU64,
    completion_events: AtomicU64,
    maximum_semantic_lag: AtomicU64,
    vm_materialized_epochs: AtomicU64,
    world_materialized_epochs: AtomicU64,
    mmap_snapshot_loaded: AtomicBool,
    mmap_snapshot_epochs_skipped: AtomicU64,
    mmap_snapshot_bytes: AtomicU64,
}

impl Metrics {
    fn snapshot(&self) -> AdmissionEpochMetrics {
        AdmissionEpochMetrics {
            submitted_intents: self.submitted_intents.load(Ordering::Relaxed),
            backpressured_intents: self.backpressured_intents.load(Ordering::Relaxed),
            durable_epochs: self.durable_epochs.load(Ordering::Relaxed),
            applied_epochs: self.applied_epochs.load(Ordering::Relaxed),
            published_worlds: self.published_worlds.load(Ordering::Relaxed),
            accepted_intents: self.accepted_intents.load(Ordering::Relaxed),
            rejected_intents: self.rejected_intents.load(Ordering::Relaxed),
            admitted_bytes: self.admitted_bytes.load(Ordering::Relaxed),
            data_writes: self.data_writes.load(Ordering::Relaxed),
            data_syncs: self.data_syncs.load(Ordering::Relaxed),
            completion_events: self.completion_events.load(Ordering::Relaxed),
            maximum_semantic_lag: self.maximum_semantic_lag.load(Ordering::Relaxed),
            vm_materialized_epochs: self.vm_materialized_epochs.load(Ordering::Relaxed),
            world_materialized_epochs: self.world_materialized_epochs.load(Ordering::Relaxed),
            mmap_snapshot_loaded: self.mmap_snapshot_loaded.load(Ordering::Relaxed),
            mmap_snapshot_epochs_skipped: self
                .mmap_snapshot_epochs_skipped
                .load(Ordering::Relaxed),
            mmap_snapshot_bytes: self.mmap_snapshot_bytes.load(Ordering::Relaxed),
        }
    }

    fn observe_lag(&self) {
        let lag = self
            .durable_epochs
            .load(Ordering::Acquire)
            .saturating_sub(self.applied_epochs.load(Ordering::Acquire));
        self.maximum_semantic_lag.fetch_max(lag, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdmissionMaterializer {
    World,
    TokenVm,
}

enum EpochMaterializer {
    World,
    TokenVm(VmEpochMaterializer),
}

struct StagedIntent {
    intent: QueuedIntent,
    admission: mpsc::Sender<Result<AdmissionEpochReceipt, String>>,
    outcome: mpsc::Sender<AdmissionEpochTicketOutcome>,
}

enum Command {
    Intent(StagedIntent),
    Epoch(Vec<StagedIntent>),
    Barrier(mpsc::Sender<Result<(), String>>),
}

struct EpochBatch {
    id: u64,
    staged: Vec<StagedIntent>,
}

#[cfg(target_os = "linux")]
struct PendingAdmission {
    batch: EpochBatch,
    start_offset: u64,
    record_len: usize,
    pending: PendingIoUringEpoch,
}

pub struct AdmissionEpochController {
    current: Arc<RwLock<Arc<World>>>,
    validators: Arc<RwLock<Vec<Validator>>>,
    sender: Mutex<Option<SyncSender<Command>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    metrics: Arc<Metrics>,
    closed: Arc<AtomicBool>,
    journal_path: PathBuf,
    materializer_kind: AdmissionMaterializer,
    _writer_lease: WriterLease,
}

impl AdmissionEpochController {
    pub fn open(
        path: impl AsRef<Path>,
        capacity: usize,
        max_batch: usize,
        ring_entries: u32,
    ) -> Result<Self, AdmissionEpochOpenError> {
        Self::open_with_validators_and_window(
            path,
            capacity,
            max_batch,
            ring_entries,
            1,
            Vec::new(),
        )
    }

    pub fn open_vm(
        path: impl AsRef<Path>,
        capacity: usize,
        max_batch: usize,
        ring_entries: u32,
    ) -> Result<Self, AdmissionEpochOpenError> {
        Self::open_with_materializer(
            path,
            capacity,
            max_batch,
            ring_entries,
            1,
            Vec::new(),
            AdmissionMaterializer::TokenVm,
        )
    }

    pub fn open_vm_with_window(
        path: impl AsRef<Path>,
        capacity: usize,
        max_batch: usize,
        ring_entries: u32,
        max_unapplied_epochs: usize,
    ) -> Result<Self, AdmissionEpochOpenError> {
        Self::open_with_materializer(
            path,
            capacity,
            max_batch,
            ring_entries,
            max_unapplied_epochs,
            Vec::new(),
            AdmissionMaterializer::TokenVm,
        )
    }

    pub fn open_with_window(
        path: impl AsRef<Path>,
        capacity: usize,
        max_batch: usize,
        ring_entries: u32,
        max_unapplied_epochs: usize,
    ) -> Result<Self, AdmissionEpochOpenError> {
        Self::open_with_validators_and_window(
            path,
            capacity,
            max_batch,
            ring_entries,
            max_unapplied_epochs,
            Vec::new(),
        )
    }

    pub fn open_with_validators(
        path: impl AsRef<Path>,
        capacity: usize,
        max_batch: usize,
        ring_entries: u32,
        validators: Vec<Validator>,
    ) -> Result<Self, AdmissionEpochOpenError> {
        Self::open_with_validators_and_window(
            path,
            capacity,
            max_batch,
            ring_entries,
            1,
            validators,
        )
    }

    pub fn open_with_validators_and_window(
        path: impl AsRef<Path>,
        capacity: usize,
        max_batch: usize,
        ring_entries: u32,
        max_unapplied_epochs: usize,
        validators: Vec<Validator>,
    ) -> Result<Self, AdmissionEpochOpenError> {
        Self::open_with_materializer(
            path,
            capacity,
            max_batch,
            ring_entries,
            max_unapplied_epochs,
            validators,
            AdmissionMaterializer::World,
        )
    }

    pub fn open_vm_with_validators_and_window(
        path: impl AsRef<Path>,
        capacity: usize,
        max_batch: usize,
        ring_entries: u32,
        max_unapplied_epochs: usize,
        validators: Vec<Validator>,
    ) -> Result<Self, AdmissionEpochOpenError> {
        Self::open_with_materializer(
            path,
            capacity,
            max_batch,
            ring_entries,
            max_unapplied_epochs,
            validators,
            AdmissionMaterializer::TokenVm,
        )
    }

    fn open_with_materializer(
        path: impl AsRef<Path>,
        capacity: usize,
        max_batch: usize,
        ring_entries: u32,
        max_unapplied_epochs: usize,
        validators: Vec<Validator>,
        materializer: AdmissionMaterializer,
    ) -> Result<Self, AdmissionEpochOpenError> {
        if capacity == 0 || max_batch == 0 || max_unapplied_epochs == 0 {
            return Err(AdmissionEpochOpenError::Config(
                "capacity, maximum batch, and maximum unapplied epochs must be nonzero".to_owned(),
            ));
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (path.as_ref(), ring_entries, validators);
            return Err(AdmissionEpochOpenError::Config(
                "the io_uring admission journal requires Linux".to_owned(),
            ));
        }
        #[cfg(target_os = "linux")]
        {
            let path = path.as_ref().to_path_buf();
            let materializer_kind = materializer;
            let lease = WriterLease::acquire(&path)?;
            let recovered = recover_journal(
                &path,
                materializer_kind == AdmissionMaterializer::TokenVm && validators.is_empty(),
            )?;
            let validator_store = Arc::new(RwLock::new(validators));
            let (replayed, materializer) = replay_epochs(
                recovered.snapshot.clone(),
                &recovered.epochs,
                &validator_store
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                materializer_kind,
            )?;
            let current = Arc::new(RwLock::new(replayed));
            let io = IoUringEpochFileIo::open(&path, ring_entries)?;
            let metrics = Arc::new(Metrics::default());
            metrics
                .durable_epochs
                .store(recovered.total_epochs, Ordering::Release);
            metrics
                .applied_epochs
                .store(recovered.total_epochs, Ordering::Release);
            if let Some(snapshot) = &recovered.snapshot {
                metrics.mmap_snapshot_loaded.store(true, Ordering::Release);
                metrics
                    .mmap_snapshot_epochs_skipped
                    .store(snapshot.epoch_count(), Ordering::Release);
                metrics
                    .mmap_snapshot_bytes
                    .store(snapshot.snapshot_bytes(), Ordering::Release);
            }
            metrics
                .published_worlds
                .store(current.read().unwrap().version(), Ordering::Release);
            let closed = Arc::new(AtomicBool::new(false));
            let (sender, receiver) = mpsc::sync_channel(capacity);
            let worker_current = current.clone();
            let worker_validators = validator_store.clone();
            let worker_metrics = metrics.clone();
            let worker_closed = closed.clone();
            let worker = thread::Builder::new()
                .name("forthdb-admission-materializer".to_owned())
                .spawn(move || {
                    run_worker(
                        io,
                        recovered.file_len,
                        recovered.total_epochs + 1,
                        receiver,
                        max_batch,
                        max_unapplied_epochs,
                        worker_current,
                        worker_validators,
                        worker_metrics,
                        materializer,
                    );
                    worker_closed.store(true, Ordering::Release);
                })
                .map_err(AdmissionEpochOpenError::Io)?;
            Ok(Self {
                current,
                validators: validator_store,
                sender: Mutex::new(Some(sender)),
                worker: Mutex::new(Some(worker)),
                metrics,
                closed,
                journal_path: path,
                materializer_kind,
                _writer_lease: lease,
            })
        }
    }

    pub fn snapshot(&self) -> Arc<World> {
        self.current
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Validators registered after open affect future epochs only. Callers
    /// requiring validator-dependent replay must supply them to
    /// `open_with_validators` before journal recovery begins.
    pub fn register_validator<F>(&self, validator: F)
    where
        F: Fn(&CandidateWorld) -> Result<(), String> + Send + Sync + 'static,
    {
        self.validators
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Arc::new(validator));
    }

    pub fn submit(
        &self,
        intent: QueuedIntent,
    ) -> Result<AdmissionEpochTicket, AdmissionEpochSubmitError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(AdmissionEpochSubmitError::Closed(intent));
        }
        let (admission_tx, admission_rx) = mpsc::channel();
        let (outcome_tx, outcome_rx) = mpsc::channel();
        let staged = StagedIntent {
            intent,
            admission: admission_tx,
            outcome: outcome_tx,
        };
        let guard = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(sender) = guard.as_ref() else {
            return Err(AdmissionEpochSubmitError::Closed(staged.intent));
        };
        match sender.try_send(Command::Intent(staged)) {
            Ok(()) => {
                self.metrics
                    .submitted_intents
                    .fetch_add(1, Ordering::Relaxed);
                Ok(AdmissionEpochTicket {
                    admission: admission_rx,
                    outcome: outcome_rx,
                })
            }
            Err(TrySendError::Full(Command::Intent(staged))) => {
                self.metrics
                    .backpressured_intents
                    .fetch_add(1, Ordering::Relaxed);
                Err(AdmissionEpochSubmitError::Full(staged.intent))
            }
            Err(TrySendError::Disconnected(Command::Intent(staged))) => {
                Err(AdmissionEpochSubmitError::Closed(staged.intent))
            }
            Err(_) => unreachable!("only intent commands are submitted through this path"),
        }
    }

    pub fn submit_epoch(
        &self,
        intents: Vec<QueuedIntent>,
    ) -> Result<Vec<AdmissionEpochTicket>, AdmissionEpochBatchSubmitError> {
        if intents.is_empty() {
            return Ok(Vec::new());
        }
        if self.closed.load(Ordering::Acquire) {
            return Err(AdmissionEpochBatchSubmitError::Closed(intents));
        }
        let mut staged = Vec::with_capacity(intents.len());
        let mut tickets = Vec::with_capacity(intents.len());
        for intent in intents {
            let (admission_tx, admission_rx) = mpsc::channel();
            let (outcome_tx, outcome_rx) = mpsc::channel();
            staged.push(StagedIntent {
                intent,
                admission: admission_tx,
                outcome: outcome_tx,
            });
            tickets.push(AdmissionEpochTicket {
                admission: admission_rx,
                outcome: outcome_rx,
            });
        }
        let guard = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(sender) = guard.as_ref() else {
            return Err(AdmissionEpochBatchSubmitError::Closed(
                staged.into_iter().map(|staged| staged.intent).collect(),
            ));
        };
        match sender.try_send(Command::Epoch(staged)) {
            Ok(()) => {
                self.metrics
                    .submitted_intents
                    .fetch_add(tickets.len() as u64, Ordering::Relaxed);
                Ok(tickets)
            }
            Err(TrySendError::Full(Command::Epoch(staged))) => {
                self.metrics
                    .backpressured_intents
                    .fetch_add(staged.len() as u64, Ordering::Relaxed);
                Err(AdmissionEpochBatchSubmitError::Full(
                    staged.into_iter().map(|staged| staged.intent).collect(),
                ))
            }
            Err(TrySendError::Disconnected(Command::Epoch(staged))) => {
                Err(AdmissionEpochBatchSubmitError::Closed(
                    staged.into_iter().map(|staged| staged.intent).collect(),
                ))
            }
            Err(_) => unreachable!("batch submission preserves its command shape"),
        }
    }

    pub fn flush(&self) -> Result<(), String> {
        let guard = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sender = guard
            .as_ref()
            .ok_or_else(|| "admission controller closed".to_owned())?;
        let (completed, receiver) = mpsc::channel();
        sender
            .send(Command::Barrier(completed))
            .map_err(|_| "admission controller closed".to_owned())?;
        receiver
            .recv()
            .map_err(|_| "admission worker stopped at barrier".to_owned())?
    }

    pub fn metrics(&self) -> AdmissionEpochMetrics {
        self.metrics.snapshot()
    }

    /// Persist a stable, offset-based image of the current token-VM query root.
    /// The admission journal remains authoritative; invalid or stale images are
    /// ignored during the next open.
    pub fn write_mmap_snapshot(&self) -> Result<MmapSnapshotMetadata, String> {
        if self.materializer_kind != AdmissionMaterializer::TokenVm {
            return Err("mmap VM snapshots require the token VM materializer".to_owned());
        }
        if !self
            .validators
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
        {
            return Err("mmap VM snapshots do not yet capture host validators".to_owned());
        }
        self.flush()?;
        let world = self.snapshot();
        world.materialize_query_projection();
        let journal = std::fs::read(&self.journal_path).map_err(|error| error.to_string())?;
        MmapVmSnapshot::create(
            &self.journal_path,
            &journal,
            self.metrics().durable_epochs,
            &world,
            world.vm_query(),
        )
    }

    pub fn shutdown(&self) {
        self.sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = worker.join();
        }
        self.closed.store(true, Ordering::Release);
    }
}

impl Drop for AdmissionEpochController {
    fn drop(&mut self) {
        self.sender
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = worker.join();
        }
    }
}

#[cfg(target_os = "linux")]
fn run_worker(
    mut io: IoUringEpochFileIo,
    mut file_len: u64,
    mut next_epoch: u64,
    receiver: Receiver<Command>,
    max_batch: usize,
    max_unapplied_epochs: usize,
    current: Arc<RwLock<Arc<World>>>,
    validators: Arc<RwLock<Vec<Validator>>>,
    metrics: Arc<Metrics>,
    mut materializer: EpochMaterializer,
) {
    let mut pending_commands = VecDeque::new();
    let mut durable_backlog = VecDeque::new();
    let mut barrier = None;
    let mut disconnected = false;
    loop {
        while durable_backlog.len() < max_unapplied_epochs && barrier.is_none() && !disconnected {
            let command = match pending_commands.pop_front() {
                Some(command) => Some(command),
                None if durable_backlog.is_empty() => match receiver.recv() {
                    Ok(command) => Some(command),
                    Err(_) => {
                        disconnected = true;
                        None
                    }
                },
                None => match receiver.try_recv() {
                    Ok(command) => Some(command),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        None
                    }
                },
            };
            let Some(command) = command else {
                break;
            };
            let staged = match command {
                Command::Barrier(completed) => {
                    barrier = Some(completed);
                    break;
                }
                Command::Intent(first) => {
                    claim_batch(first, &receiver, &mut pending_commands, max_batch)
                }
                Command::Epoch(batch) => batch,
            };
            let epoch = EpochBatch {
                id: next_epoch,
                staged,
            };
            next_epoch += 1;
            let pending = match submit_admission(&mut io, file_len, epoch) {
                Ok(pending) => pending,
                Err((batch, error)) => {
                    fail_batch(batch, error.clone());
                    fail_durable_backlog(&mut durable_backlog, error);
                    return;
                }
            };
            match complete_admission(&mut io, &mut file_len, pending, &metrics) {
                Ok(durable) => {
                    notify_admitted(&durable);
                    durable_backlog.push_back(durable);
                }
                Err((batch, error)) => {
                    fail_batch(batch, error.clone());
                    fail_durable_backlog(&mut durable_backlog, error);
                    return;
                }
            }
        }

        if let Some(durable) = durable_backlog.pop_front() {
            let mut pending_successor = None;
            if barrier.is_none() && !disconnected && durable_backlog.len() < max_unapplied_epochs {
                let command = match pending_commands.pop_front() {
                    Some(command) => Some(command),
                    None => match receiver.try_recv() {
                        Ok(command) => Some(command),
                        Err(TryRecvError::Empty) => None,
                        Err(TryRecvError::Disconnected) => {
                            disconnected = true;
                            None
                        }
                    },
                };
                if let Some(command) = command {
                    let staged = match command {
                        Command::Barrier(completed) => {
                            barrier = Some(completed);
                            None
                        }
                        Command::Intent(first) => Some(claim_batch(
                            first,
                            &receiver,
                            &mut pending_commands,
                            max_batch,
                        )),
                        Command::Epoch(batch) => Some(batch),
                    };
                    if let Some(staged) = staged {
                        let epoch = EpochBatch {
                            id: next_epoch,
                            staged,
                        };
                        next_epoch += 1;
                        match submit_admission(&mut io, file_len, epoch) {
                            Ok(pending) => pending_successor = Some(pending),
                            Err((batch, error)) => {
                                fail_batch(batch, error.clone());
                                fail_outcomes(durable, error.clone());
                                fail_durable_backlog(&mut durable_backlog, error);
                                return;
                            }
                        }
                    }
                }
            }

            if let Err((batch, error)) =
                materialize(durable, &current, &validators, &metrics, &mut materializer)
            {
                fail_outcomes(batch, error.clone());
                if let Some(pending) = pending_successor {
                    match complete_admission(&mut io, &mut file_len, pending, &metrics) {
                        Ok(batch) => {
                            notify_admitted(&batch);
                            fail_outcomes(batch, error.clone());
                        }
                        Err((batch, failure)) => fail_batch(batch, failure),
                    }
                }
                fail_durable_backlog(&mut durable_backlog, error);
                return;
            }

            if let Some(pending) = pending_successor {
                match complete_admission(&mut io, &mut file_len, pending, &metrics) {
                    Ok(durable) => {
                        notify_admitted(&durable);
                        durable_backlog.push_back(durable);
                    }
                    Err((batch, error)) => {
                        fail_batch(batch, error.clone());
                        fail_durable_backlog(&mut durable_backlog, error);
                        return;
                    }
                }
            }
            continue;
        }

        if let Some(completed) = barrier.take() {
            let _ = completed.send(Ok(()));
            continue;
        }
        if disconnected {
            break;
        }
    }
}

fn claim_batch(
    first: StagedIntent,
    receiver: &Receiver<Command>,
    pending: &mut VecDeque<Command>,
    max_batch: usize,
) -> Vec<StagedIntent> {
    let mut batch = vec![first];
    while batch.len() < max_batch {
        match receiver.try_recv() {
            Ok(Command::Intent(intent)) => batch.push(intent),
            Ok(command @ Command::Epoch(_)) => {
                pending.push_back(command);
                break;
            }
            Ok(command @ Command::Barrier(_)) => {
                pending.push_back(command);
                break;
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
    batch
}

#[cfg(target_os = "linux")]
fn submit_admission(
    io: &mut IoUringEpochFileIo,
    start_offset: u64,
    batch: EpochBatch,
) -> Result<PendingAdmission, (EpochBatch, String)> {
    let record = encode_epoch(&batch);
    let record_len = record.len();
    match io.submit_contiguous_epoch(start_offset, &[record]) {
        Ok(pending) => Ok(PendingAdmission {
            batch,
            start_offset,
            record_len,
            pending,
        }),
        Err((phase, error)) => Err((
            batch,
            format!("admission submission failed during {phase:?}: {error}"),
        )),
    }
}

#[cfg(target_os = "linux")]
fn complete_admission(
    io: &mut IoUringEpochFileIo,
    file_len: &mut u64,
    pending: PendingAdmission,
    metrics: &Metrics,
) -> Result<EpochBatch, (EpochBatch, String)> {
    let PendingAdmission {
        batch,
        start_offset,
        record_len,
        pending,
    } = pending;
    let (transport, result) = io.complete_contiguous_epoch(pending);
    metrics
        .data_writes
        .fetch_add(transport.data_writes, Ordering::Relaxed);
    metrics
        .data_syncs
        .fetch_add(transport.data_syncs, Ordering::Relaxed);
    metrics
        .completion_events
        .fetch_add(transport.completion_events, Ordering::Relaxed);
    if let Err((phase, error)) = result {
        let repair = io
            .set_len(EpochIoPhase::RepairTruncate, start_offset)
            .and_then(|_| io.sync_data(EpochIoPhase::RepairSync));
        let message = match repair {
            Ok(()) => format!("admission failed during {phase:?} and was repaired: {error}"),
            Err(repair) => {
                format!("admission failed during {phase:?}: {error}; repair failed: {repair}")
            }
        };
        return Err((batch, message));
    }
    *file_len += record_len as u64;
    metrics.durable_epochs.fetch_add(1, Ordering::AcqRel);
    metrics
        .admitted_bytes
        .fetch_add(record_len as u64, Ordering::Relaxed);
    metrics.observe_lag();
    Ok(batch)
}

fn notify_admitted(batch: &EpochBatch) {
    for (position, staged) in batch.staged.iter().enumerate() {
        let _ = staged.admission.send(Ok(AdmissionEpochReceipt {
            epoch_id: batch.id,
            position,
        }));
    }
}

fn materialize(
    batch: EpochBatch,
    current: &RwLock<Arc<World>>,
    validators: &RwLock<Vec<Validator>>,
    metrics: &Metrics,
    materializer: &mut EpochMaterializer,
) -> Result<(), (EpochBatch, String)> {
    let base = current
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let intents = batch
        .staged
        .iter()
        .map(|staged| staged.intent.clone())
        .collect();
    let validator_snapshot = validators
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let plan = match catch_unwind(AssertUnwindSafe(|| match materializer {
        EpochMaterializer::World => Ok((
            derive_epoch_world(base, intents, &validator_snapshot),
            false,
        )),
        EpochMaterializer::TokenVm(vm) => vm.materialize(base, intents, &validator_snapshot),
    })) {
        Ok(Ok((plan, used_vm))) => {
            if used_vm {
                metrics
                    .vm_materialized_epochs
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                metrics
                    .world_materialized_epochs
                    .fetch_add(1, Ordering::Relaxed);
            }
            plan
        }
        Ok(Err(error)) => return Err((batch, error)),
        Err(payload) => {
            return Err((
                batch,
                format!(
                    "semantic materializer panicked after durable admission: {}",
                    panic_message(payload)
                ),
            ));
        }
    };
    if !plan.is_empty() {
        *current
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = plan.tail();
        metrics.published_worlds.fetch_add(1, Ordering::Relaxed);
    }
    for (position, (staged, outcome)) in batch
        .staged
        .into_iter()
        .zip(plan.outcomes().iter())
        .enumerate()
    {
        let receipt = AdmissionEpochReceipt {
            epoch_id: batch.id,
            position,
        };
        let result = match outcome {
            EpochOutcome::Accepted(accepted) => {
                metrics.accepted_intents.fetch_add(1, Ordering::Relaxed);
                AdmissionEpochTicketOutcome::Accepted {
                    receipt,
                    world: accepted.world(),
                    entities: accepted.entities().clone(),
                }
            }
            EpochOutcome::Rejected(rejected) => {
                metrics.rejected_intents.fetch_add(1, Ordering::Relaxed);
                AdmissionEpochTicketOutcome::Rejected {
                    receipt,
                    error: DurableTicketRejection::from_intent(rejected.error()),
                }
            }
        };
        let _ = staged.outcome.send(result);
    }
    metrics.applied_epochs.fetch_add(1, Ordering::AcqRel);
    Ok(())
}

fn fail_batch(batch: EpochBatch, error: String) {
    for staged in batch.staged {
        let _ = staged.admission.send(Err(error.clone()));
        let _ = staged
            .outcome
            .send(AdmissionEpochTicketOutcome::Failed(error.clone()));
    }
}

fn fail_outcomes(batch: EpochBatch, error: String) {
    for staged in batch.staged {
        let _ = staged
            .outcome
            .send(AdmissionEpochTicketOutcome::Failed(error.clone()));
    }
}

fn fail_durable_backlog(backlog: &mut VecDeque<EpochBatch>, error: String) {
    while let Some(batch) = backlog.pop_front() {
        fail_outcomes(batch, error.clone());
    }
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

struct RecoveredJournal {
    epochs: Vec<Vec<QueuedIntent>>,
    file_len: u64,
    total_epochs: u64,
    snapshot: Option<Arc<MmapVmSnapshot>>,
}

fn recover_journal(
    path: &Path,
    allow_mmap_snapshot: bool,
) -> Result<RecoveredJournal, AdmissionEpochOpenError> {
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)?;
    if file.metadata()?.len() == 0 {
        file.write_all(JOURNAL_MAGIC)?;
        file.write_all(&JOURNAL_VERSION.to_le_bytes())?;
        file.write_all(&0u32.to_le_bytes())?;
        file.sync_data()?;
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if bytes.len() < JOURNAL_HEADER_LEN || &bytes[..8] != JOURNAL_MAGIC {
        return Err(AdmissionEpochOpenError::Format(
            "invalid journal header".to_owned(),
        ));
    }
    if u32::from_le_bytes(bytes[8..12].try_into().unwrap()) != JOURNAL_VERSION {
        return Err(AdmissionEpochOpenError::Format(
            "unsupported journal version".to_owned(),
        ));
    }
    if allow_mmap_snapshot {
        if let Ok(snapshot) = MmapVmSnapshot::open(path, &bytes) {
            if snapshot.journal_offset() == bytes.len() as u64 {
                return Ok(RecoveredJournal {
                    epochs: Vec::new(),
                    file_len: bytes.len() as u64,
                    total_epochs: snapshot.epoch_count(),
                    snapshot: Some(snapshot),
                });
            }
        }
    }
    let mut offset = JOURNAL_HEADER_LEN;
    let mut epochs = Vec::new();
    while offset < bytes.len() {
        if bytes.len() - offset < EPOCH_PREFIX_LEN {
            break;
        }
        if &bytes[offset..offset + 4] != EPOCH_MAGIC {
            return Err(AdmissionEpochOpenError::Format(format!(
                "invalid epoch magic at {offset}"
            )));
        }
        let payload_len =
            u64::from_le_bytes(bytes[offset + 4..offset + 12].try_into().unwrap()) as usize;
        if payload_len > MAX_EPOCH_BYTES {
            return Err(AdmissionEpochOpenError::Format(
                "epoch exceeds size limit".to_owned(),
            ));
        }
        let checksum = u64::from_le_bytes(bytes[offset + 12..offset + 20].try_into().unwrap());
        let end = offset
            .checked_add(EPOCH_PREFIX_LEN + payload_len + EPOCH_TRAILER.len())
            .ok_or_else(|| AdmissionEpochOpenError::Format("epoch length overflow".to_owned()))?;
        if end > bytes.len() {
            break;
        }
        if &bytes[end - 4..end] != EPOCH_TRAILER {
            return Err(AdmissionEpochOpenError::Format(format!(
                "invalid epoch trailer at {offset}"
            )));
        }
        let payload = &bytes[offset + EPOCH_PREFIX_LEN..end - 4];
        if digest(payload) != checksum {
            return Err(AdmissionEpochOpenError::Format(format!(
                "epoch checksum mismatch at {offset}"
            )));
        }
        let (epoch_id, intents) =
            decode_epoch_payload(payload).map_err(AdmissionEpochOpenError::Format)?;
        let expected_epoch = epochs.len() as u64 + 1;
        if epoch_id != expected_epoch {
            return Err(AdmissionEpochOpenError::Format(format!(
                "expected admission epoch {expected_epoch}, found {epoch_id}"
            )));
        }
        epochs.push(intents);
        offset = end;
    }
    if offset != bytes.len() {
        file.set_len(offset as u64)?;
        file.sync_data()?;
    }
    Ok(RecoveredJournal {
        total_epochs: epochs.len() as u64,
        epochs,
        file_len: offset as u64,
        snapshot: None,
    })
}

fn replay_epochs(
    snapshot: Option<Arc<MmapVmSnapshot>>,
    epochs: &[Vec<QueuedIntent>],
    validators: &[Validator],
    materializer: AdmissionMaterializer,
) -> Result<(Arc<World>, EpochMaterializer), AdmissionEpochOpenError> {
    let mut world = snapshot
        .as_ref()
        .map_or_else(|| Arc::new(World::genesis()), |snapshot| World::from_mmap(snapshot.clone()));
    let mut materializer = match materializer {
        AdmissionMaterializer::World => EpochMaterializer::World,
        AdmissionMaterializer::TokenVm => {
            let vm = if let Some(snapshot) = snapshot {
                VmEpochMaterializer::from_mmap(snapshot)
                    .map_err(AdmissionEpochOpenError::Format)?
            } else {
                VmEpochMaterializer::new(world.next_entity())
            };
            EpochMaterializer::TokenVm(vm)
        }
    };
    for intents in epochs {
        world = match &mut materializer {
            EpochMaterializer::World => {
                derive_epoch_world(world, intents.clone(), validators).tail()
            }
            EpochMaterializer::TokenVm(vm) => vm
                .materialize(world, intents.clone(), validators)
                .map_err(AdmissionEpochOpenError::Format)?
                .0
                .tail(),
        };
    }
    Ok((world, materializer))
}

fn encode_epoch(batch: &EpochBatch) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&batch.id.to_le_bytes());
    payload.extend_from_slice(&(batch.staged.len() as u32).to_le_bytes());
    for staged in &batch.staged {
        let mut encoded = Vec::new();
        encode_queued_intent(&staged.intent, &mut encoded);
        payload.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
        payload.extend_from_slice(&encoded);
    }
    let mut record = Vec::with_capacity(EPOCH_PREFIX_LEN + payload.len() + 4);
    record.extend_from_slice(EPOCH_MAGIC);
    record.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    record.extend_from_slice(&digest(&payload).to_le_bytes());
    record.extend_from_slice(&payload);
    record.extend_from_slice(EPOCH_TRAILER);
    record
}

fn decode_epoch_payload(payload: &[u8]) -> Result<(u64, Vec<QueuedIntent>), String> {
    let mut cursor = Cursor::new(payload);
    let epoch_id = read_u64(&mut cursor)?;
    let count = read_u32(&mut cursor)? as usize;
    let mut intents = Vec::with_capacity(count);
    for _ in 0..count {
        let length = read_u32(&mut cursor)? as usize;
        let start = cursor.position() as usize;
        let end = start
            .checked_add(length)
            .ok_or_else(|| "intent length overflow".to_owned())?;
        if end > payload.len() {
            return Err("truncated intent in admission epoch".to_owned());
        }
        let mut intent_cursor = Cursor::new(&payload[start..end]);
        intents.push(decode_queued_intent(&mut intent_cursor)?);
        if intent_cursor.position() as usize != length {
            return Err("intent decoder did not consume its canonical bytes".to_owned());
        }
        cursor.set_position(end as u64);
    }
    if cursor.position() as usize != payload.len() {
        return Err("trailing admission epoch payload bytes".to_owned());
    }
    Ok((epoch_id, intents))
}

fn read_u32(cursor: &mut Cursor<&[u8]>) -> Result<u32, String> {
    let mut bytes = [0; 4];
    cursor
        .read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(cursor: &mut Cursor<&[u8]>) -> Result<u64, String> {
    let mut bytes = [0; 8];
    cursor
        .read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(u64::from_le_bytes(bytes))
}

fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(CHECKSUM_OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(CHECKSUM_PRIME)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::time::{Duration, Instant};

    static TEST_PATH: AtomicU64 = AtomicU64::new(1);

    fn path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "forthdb-admission-{label}-{}-{}.fdb",
            std::process::id(),
            TEST_PATH.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn batch(id: u64, intents: Vec<QueuedIntent>) -> EpochBatch {
        EpochBatch {
            id,
            staged: intents
                .into_iter()
                .map(|intent| {
                    let (admission, _) = mpsc::channel();
                    let (outcome, _) = mpsc::channel();
                    StagedIntent {
                        intent,
                        admission,
                        outcome,
                    }
                })
                .collect(),
        }
    }

    #[test]
    fn admission_epoch_codec_preserves_intents_and_boundary() {
        let mut first = QueuedIntent::new();
        let entity = first.entity();
        first.define(
            SlotId::new("codec/name"),
            IntentFact::new(entity, Predicate::new("name"), Literal::new("Ada")),
        );
        let (admission, _) = mpsc::channel();
        let (outcome, _) = mpsc::channel();
        let batch = EpochBatch {
            id: 42,
            staged: vec![StagedIntent {
                intent: first.clone(),
                admission,
                outcome,
            }],
        };
        let record = encode_epoch(&batch);
        let payload_len = u64::from_le_bytes(record[4..12].try_into().unwrap()) as usize;
        let payload = &record[EPOCH_PREFIX_LEN..EPOCH_PREFIX_LEN + payload_len];
        let (id, decoded) = decode_epoch_payload(payload).expect("epoch decodes");
        assert_eq!(id, 42);
        assert_eq!(decoded, vec![first]);
    }

    #[test]
    fn epoch_world_collapses_multiple_accepted_intents() {
        let base = Arc::new(World::genesis());
        let mut first = QueuedIntent::new();
        let entity = first.entity();
        first.define(
            SlotId::new("epoch/name"),
            IntentFact::new(entity, Predicate::new("name"), Literal::new("Ada")),
        );
        let mut second = QueuedIntent::new();
        second.define(
            SlotId::new("epoch/state"),
            IntentFact::new(
                EntityId::new(1),
                Predicate::new("state"),
                Literal::new("active"),
            ),
        );
        let plan = derive_epoch_world(base, vec![first, second], &[]);
        assert_eq!(
            plan.accepted_count(),
            1,
            "one frame represents one epoch world"
        );
        assert_eq!(plan.tail().version(), 1);
        assert_eq!(plan.outcomes().len(), 2);
        assert_eq!(
            plan.outcomes()[0].accepted().unwrap().world().id(),
            plan.outcomes()[1].accepted().unwrap().world().id()
        );
    }

    #[test]
    fn zero_unapplied_window_is_rejected() {
        let error = AdmissionEpochController::open_with_window(path("zero-window"), 1, 1, 2, 0)
            .err()
            .expect("zero window must fail before opening io_uring");
        assert!(matches!(error, AdmissionEpochOpenError::Config(_)));
    }

    #[test]
    fn durable_intent_epochs_replay_worlds_and_trim_an_incomplete_tail() {
        let path = path("replay");
        let initialized = recover_journal(&path, false).expect("journal initializes");
        assert_eq!(initialized.file_len, JOURNAL_HEADER_LEN as u64);

        let mut first = QueuedIntent::new();
        let entity = first.entity();
        first.define(
            SlotId::new("replay/name"),
            IntentFact::new(entity, Predicate::new("name"), Literal::new("Ada")),
        );
        let mut second = QueuedIntent::new();
        second.define(
            SlotId::new("replay/state"),
            IntentFact::new(
                EntityId::new(1),
                Predicate::new("state"),
                Literal::new("active"),
            ),
        );
        let first_record = encode_epoch(&batch(1, vec![first]));
        let second_record = encode_epoch(&batch(2, vec![second]));
        let expected_len = JOURNAL_HEADER_LEN + first_record.len() + second_record.len();
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("journal opens for append");
        file.write_all(&first_record).unwrap();
        file.write_all(&second_record).unwrap();
        file.write_all(b"partial-tail").unwrap();
        file.sync_data().unwrap();
        drop(file);

        let recovered = recover_journal(&path, false).expect("sound prefix recovers");
        assert_eq!(recovered.epochs.len(), 2);
        assert_eq!(recovered.file_len, expected_len as u64);
        assert_eq!(fs::metadata(&path).unwrap().len(), expected_len as u64);
        let (world, _) = replay_epochs(
            None,
            &recovered.epochs,
            &[],
            AdmissionMaterializer::TokenVm,
        )
        .expect("epochs materialize");
        assert_eq!(world.version(), 2);
        assert!(world.resolve(&SlotId::new("replay/name")).is_some());
        assert!(world.resolve(&SlotId::new("replay/state")).is_some());
        fs::remove_file(path).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn io_uring_token_vm_publishes_one_world_and_replays_it() {
        let path = path("io-uring");
        let controller = match AdmissionEpochController::open_vm(&path, 16, 16, 64) {
            Ok(controller) => controller,
            Err(AdmissionEpochOpenError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
                ) =>
            {
                let _ = fs::remove_file(&path);
                let _ = fs::remove_file(writer_lock_path(&path));
                return;
            }
            Err(error) => panic!("admission controller should open: {error}"),
        };
        let mut first = QueuedIntent::new();
        let entity = first.entity();
        first.define(
            SlotId::new("live/name"),
            IntentFact::new(entity, Predicate::new("name"), Literal::new("Ada")),
        );
        let mut second = QueuedIntent::new();
        second.define(
            SlotId::new("live/state"),
            IntentFact::new(
                EntityId::new(1),
                Predicate::new("state"),
                Literal::new("active"),
            ),
        );
        let mut tickets = controller
            .submit_epoch(vec![first, second])
            .expect("explicit epoch enters the journal");
        let first = tickets.remove(0);
        let second = tickets.remove(0);
        let first_receipt = first.wait_admitted().expect("first intent is durable");
        let second_receipt = second.wait_admitted().expect("second intent is durable");
        assert_eq!(first_receipt.epoch_id, second_receipt.epoch_id);
        let first_world = match first.wait().expect("first outcome resolves") {
            AdmissionEpochTicketOutcome::Accepted { world, .. } => world,
            outcome => panic!("unexpected first outcome: {outcome:?}"),
        };
        let second_world = match second.wait().expect("second outcome resolves") {
            AdmissionEpochTicketOutcome::Accepted { world, .. } => world,
            outcome => panic!("unexpected second outcome: {outcome:?}"),
        };
        assert_eq!(first_world.id(), second_world.id());
        assert_eq!(first_world.version(), 1);
        assert!(!first_world.is_query_projection_materialized());
        assert_eq!(controller.metrics().vm_materialized_epochs, 1);
        assert_eq!(controller.metrics().world_materialized_epochs, 0);
        let expected = first_world.id();
        let snapshot = controller
            .write_mmap_snapshot()
            .expect("physical VM snapshot persists");
        assert_eq!(snapshot.epoch_count, 1);
        assert_eq!(snapshot.world_id, expected);
        controller.shutdown();
        drop(controller);

        let reopened = AdmissionEpochController::open_vm(&path, 16, 16, 64)
            .expect("durable admission journal reopens");
        assert_eq!(reopened.snapshot().id(), expected);
        assert!(reopened.snapshot().is_query_projection_materialized());
        assert!(reopened.metrics().mmap_snapshot_loaded);
        assert_eq!(reopened.metrics().mmap_snapshot_epochs_skipped, 1);
        assert!(reopened
            .snapshot()
            .resolve(&SlotId::new("live/name"))
            .is_some());
        let mut next = QueuedIntent::new();
        next.define(
            SlotId::new("live/after-reopen"),
            IntentFact::new(
                EntityId::new(1),
                Predicate::new("state"),
                Literal::new("mapped"),
            ),
        );
        let ticket = reopened.submit(next).expect("mapped VM accepts a successor");
        ticket.wait_admitted().expect("successor is durable");
        match ticket.wait().expect("successor materializes") {
            AdmissionEpochTicketOutcome::Accepted { world, .. } => {
                assert!(world.resolve(&SlotId::new("live/after-reopen")).is_some());
            }
            outcome => panic!("unexpected mapped successor outcome: {outcome:?}"),
        }
        reopened.shutdown();
        drop(reopened);
        fs::remove_file(&path).unwrap();
        fs::remove_file(MmapVmSnapshot::path_for(&path)).unwrap();
        let _ = fs::remove_file(writer_lock_path(&path));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_window_admits_a_durable_backlog_before_publication() {
        let path = path("bounded-window");
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let validator_gate = gate.clone();
        let validator: Validator = Arc::new(move |_| {
            let (lock, ready) = &*validator_gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = ready.wait(released).unwrap();
            }
            Ok(())
        });
        let controller = match AdmissionEpochController::open_with_validators_and_window(
            &path,
            16,
            1,
            64,
            4,
            vec![validator],
        ) {
            Ok(controller) => controller,
            Err(AdmissionEpochOpenError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
                ) =>
            {
                let _ = fs::remove_file(&path);
                let _ = fs::remove_file(writer_lock_path(&path));
                return;
            }
            Err(error) => panic!("admission controller should open: {error}"),
        };

        let tickets = (0..4)
            .map(|index| {
                let mut intent = QueuedIntent::new();
                intent.define(
                    SlotId::new(format!("window/{index}")),
                    IntentFact::new(
                        Literal::new(index.to_string()),
                        Predicate::new("state"),
                        Literal::new("durable"),
                    ),
                );
                controller
                    .submit_epoch(vec![intent])
                    .expect("epoch enters the admission queue")
                    .pop()
                    .unwrap()
            })
            .collect::<Vec<_>>();

        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.metrics().durable_epochs < 4 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(controller.metrics().durable_epochs, 4);
        assert_eq!(controller.metrics().maximum_semantic_lag, 4);
        assert_eq!(controller.snapshot().version(), 0);

        let (lock, ready) = &*gate;
        *lock.lock().unwrap() = true;
        ready.notify_all();
        for ticket in tickets {
            ticket.wait_admitted().expect("epoch is durable");
            assert!(matches!(
                ticket.wait().expect("semantic outcome resolves"),
                AdmissionEpochTicketOutcome::Accepted { .. }
            ));
        }
        controller.flush().unwrap();
        assert_eq!(controller.snapshot().version(), 4);
        assert_eq!(controller.metrics().applied_epochs, 4);
        controller.shutdown();
        drop(controller);
        fs::remove_file(&path).unwrap();
        let _ = fs::remove_file(writer_lock_path(&path));
    }
}
