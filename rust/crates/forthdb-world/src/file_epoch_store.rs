use super::*;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const FILE_MAGIC: &[u8; 8] = b"FTHDB001";
const FILE_VERSION: u32 = 1;
const FILE_FLAGS: u32 = 0;
const FILE_HEADER_LEN: usize = 16;
const FRAME_MAGIC: &[u8; 4] = b"FRM1";
const FRAME_TRAILER: &[u8; 4] = b"END1";
const FRAME_PREFIX_LEN: usize = 20;
const FRAME_TRAILER_LEN: usize = 4;
const MAX_FRAME_BYTES: u64 = 64 * 1024 * 1024;
const MAX_OPERATIONS_PER_FRAME: u64 = 1_000_000;
const CHECKSUM_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const CHECKSUM_PRIME: u64 = 0x100000001b3;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EpochIoPhase {
    EpochStart,
    EpochWrite,
    EpochSync,
    RepairTruncate,
    RepairSync,
    VerifyLength,
    VerifyRead,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileEpochState {
    Healthy,
    Repairing,
    Poisoned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileEpochSyncPolicy {
    PerFrame,
    PerEpoch,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EpochPersistMetrics {
    pub data_writes: u64,
    pub data_syncs: u64,
    pub bytes_written: u64,
    pub submission_calls: u64,
    pub completion_events: u64,
    pub maximum_in_flight_writes: u64,
    pub iovecs_submitted: u64,
    pub arena_bytes_copied: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FileEpochMetrics {
    pub epoch_attempts: u64,
    pub epochs_committed: u64,
    pub frames_committed: u64,
    pub data_writes: u64,
    pub data_syncs: u64,
    pub repair_syncs: u64,
    pub repairs_succeeded: u64,
    pub repairs_failed: u64,
    pub bytes_written: u64,
    pub submission_calls: u64,
    pub completion_events: u64,
    pub maximum_in_flight_writes: u64,
    pub iovecs_submitted: u64,
    pub arena_bytes_copied: u64,
}

impl FileEpochMetrics {
    fn record_persist(&mut self, metrics: EpochPersistMetrics) {
        self.data_writes += metrics.data_writes;
        self.data_syncs += metrics.data_syncs;
        self.bytes_written += metrics.bytes_written;
        self.submission_calls += metrics.submission_calls;
        self.completion_events += metrics.completion_events;
        self.maximum_in_flight_writes = self
            .maximum_in_flight_writes
            .max(metrics.maximum_in_flight_writes);
        self.iovecs_submitted += metrics.iovecs_submitted;
        self.arena_bytes_copied += metrics.arena_bytes_copied;
    }
}

/// Narrow syscall boundary used by the ordinary-file epoch state machine.
///
/// Production delegates to a real `std::fs::File`. Tests wrap the same real
/// file and inject deterministic outcomes at named semantic phases.
pub trait EpochFileIo: Send {
    /// Optional whole-epoch transport hook.
    ///
    /// Implementations such as io_uring receive the independently encoded
    /// canonical records before the ordinary-file control copies or writes
    /// them. Returning `None` selects the established `write_at`/`sync_data`
    /// path. Returning `Some` must not return until every submitted operation
    /// has completed and all borrowed record buffers are no longer referenced.
    fn persist_epoch(
        &mut self,
        _start_offset: u64,
        _records: &[Vec<u8>],
    ) -> Option<(
        EpochPersistMetrics,
        Result<(), (EpochIoPhase, std::io::Error)>,
    )> {
        None
    }

    fn len(&mut self, phase: EpochIoPhase) -> std::io::Result<u64>;
    fn write_at(
        &mut self,
        phase: EpochIoPhase,
        offset: u64,
        bytes: &[u8],
    ) -> std::io::Result<usize>;
    fn sync_data(&mut self, phase: EpochIoPhase) -> std::io::Result<()>;
    fn set_len(&mut self, phase: EpochIoPhase, len: u64) -> std::io::Result<()>;
    fn read_all(&mut self, phase: EpochIoPhase) -> std::io::Result<Vec<u8>>;
}

#[derive(Debug)]
pub struct StdEpochFileIo {
    file: File,
}

impl StdEpochFileIo {
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self {
            file: OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(path)?,
        })
    }
}

impl EpochFileIo for StdEpochFileIo {
    fn len(&mut self, _phase: EpochIoPhase) -> std::io::Result<u64> {
        self.file.metadata().map(|metadata| metadata.len())
    }

    fn write_at(
        &mut self,
        _phase: EpochIoPhase,
        offset: u64,
        bytes: &[u8],
    ) -> std::io::Result<usize> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write(bytes)
    }

    fn sync_data(&mut self, _phase: EpochIoPhase) -> std::io::Result<()> {
        self.file.sync_data()
    }

    fn set_len(&mut self, _phase: EpochIoPhase, len: u64) -> std::io::Result<()> {
        self.file.set_len(len)
    }

    fn read_all(&mut self, _phase: EpochIoPhase) -> std::io::Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

#[derive(Debug)]
pub enum FileEpochStoreError {
    File(FileCommitStoreError),
    Io {
        phase: EpochIoPhase,
        source: std::io::Error,
    },
    EpochRepaired {
        phase: EpochIoPhase,
        source: std::io::Error,
    },
    RepairFailed {
        primary_phase: EpochIoPhase,
        primary: String,
        repair_phase: EpochIoPhase,
        repair: String,
    },
    Verification(String),
    StorePoisoned(String),
}

impl fmt::Display for FileEpochStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(error) => write!(formatter, "file epoch format failed: {error}"),
            Self::Io { phase, source } => {
                write!(formatter, "file epoch I/O failed during {phase:?}: {source}")
            }
            Self::EpochRepaired { phase, source } => write!(
                formatter,
                "file epoch failed during {phase:?} and was rolled back: {source}"
            ),
            Self::RepairFailed {
                primary_phase,
                primary,
                repair_phase,
                repair,
            } => write!(
                formatter,
                "file epoch failed during {primary_phase:?} ({primary}); repair failed during {repair_phase:?} ({repair})"
            ),
            Self::Verification(message) => write!(formatter, "file epoch verification failed: {message}"),
            Self::StorePoisoned(reason) => write!(formatter, "file epoch store is poisoned: {reason}"),
        }
    }
}

impl Error for FileEpochStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::File(error) => Some(error),
            Self::Io { source, .. } | Self::EpochRepaired { source, .. } => Some(source),
            Self::RepairFailed { .. }
            | Self::Verification(_)
            | Self::StorePoisoned(_) => None,
        }
    }
}

impl From<FileCommitStoreError> for FileEpochStoreError {
    fn from(value: FileCommitStoreError) -> Self {
        Self::File(value)
    }
}

#[derive(Clone, Copy, Debug)]
struct EpochCheckpoint {
    start_offset: u64,
    frame_count: usize,
    world_id: WorldId,
    world_version: u64,
    prefix_digest: u64,
}

#[derive(Debug)]
struct FileInspection {
    frames: Vec<Arc<CommitFrame>>,
    last_good_offset: usize,
}

/// Ordinary-file durability epoch control.
///
/// `PerFrame` is the N-sync baseline. `PerEpoch` writes one contiguous arena
/// and performs one synchronization barrier. Both retain version-1 frames.
pub struct FileEpochStore<I: EpochFileIo = StdEpochFileIo> {
    path: PathBuf,
    io: I,
    frames: Vec<Arc<CommitFrame>>,
    policy: FileEpochSyncPolicy,
    state: FileEpochState,
    poison_reason: Option<String>,
    prefix_digest: u64,
    recovered_tail_bytes: u64,
    metrics: FileEpochMetrics,
}

impl<I: EpochFileIo> fmt::Debug for FileEpochStore<I> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileEpochStore")
            .field("path", &self.path)
            .field("frames", &self.frames.len())
            .field("policy", &self.policy)
            .field("state", &self.state)
            .field("poison_reason", &self.poison_reason)
            .field("metrics", &self.metrics)
            .finish()
    }
}

impl FileEpochStore<StdEpochFileIo> {
    pub fn open(
        path: impl AsRef<Path>,
        policy: FileEpochSyncPolicy,
    ) -> Result<Self, FileEpochStoreError> {
        let path = path.as_ref().to_path_buf();
        let cold = FileCommitStore::open(&path)?;
        let recovered_tail_bytes = cold.recovered_tail_bytes();
        drop(cold);
        let io = StdEpochFileIo::open(&path).map_err(|source| FileEpochStoreError::Io {
            phase: EpochIoPhase::VerifyRead,
            source,
        })?;
        let mut store = Self::from_io(path, io, policy)?;
        store.recovered_tail_bytes = recovered_tail_bytes;
        Ok(store)
    }
}

impl<I: EpochFileIo> FileEpochStore<I> {
    pub fn from_io(
        path: impl AsRef<Path>,
        mut io: I,
        policy: FileEpochSyncPolicy,
    ) -> Result<Self, FileEpochStoreError> {
        let bytes = io
            .read_all(EpochIoPhase::VerifyRead)
            .map_err(|source| FileEpochStoreError::Io {
                phase: EpochIoPhase::VerifyRead,
                source,
            })?;
        let inspection = inspect_file(&bytes)?;
        if inspection.last_good_offset != bytes.len() {
            return Err(FileEpochStoreError::Verification(format!(
                "opened file contains {} unverified trailing bytes",
                bytes.len() - inspection.last_good_offset
            )));
        }
        World::reconstruct(&inspection.frames).map_err(FileCommitStoreError::History)?;
        let prefix_digest = digest(&bytes);
        Ok(Self {
            path: path.as_ref().to_path_buf(),
            io,
            frames: inspection.frames,
            policy,
            state: FileEpochState::Healthy,
            poison_reason: None,
            prefix_digest,
            recovered_tail_bytes: 0,
            metrics: FileEpochMetrics::default(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn state(&self) -> FileEpochState {
        self.state
    }

    pub fn poison_reason(&self) -> Option<&str> {
        self.poison_reason.as_deref()
    }

    pub fn policy(&self) -> FileEpochSyncPolicy {
        self.policy
    }

    pub fn metrics(&self) -> FileEpochMetrics {
        self.metrics
    }

    pub fn recovered_tail_bytes(&self) -> u64 {
        self.recovered_tail_bytes
    }

    pub fn file_len(&mut self) -> Result<u64, FileEpochStoreError> {
        self.ensure_healthy()?;
        self.io
            .len(EpochIoPhase::VerifyLength)
            .map_err(|source| FileEpochStoreError::Io {
                phase: EpochIoPhase::VerifyLength,
                source,
            })
    }

    pub fn physical_frames(&mut self) -> Result<Vec<Arc<CommitFrame>>, FileEpochStoreError> {
        self.ensure_healthy()?;
        let bytes = self
            .io
            .read_all(EpochIoPhase::VerifyRead)
            .map_err(|source| FileEpochStoreError::Io {
                phase: EpochIoPhase::VerifyRead,
                source,
            })?;
        let inspection = inspect_file(&bytes)?;
        if inspection.last_good_offset != bytes.len() {
            return Err(FileEpochStoreError::Verification(
                "physical file contains an incomplete tail".to_owned(),
            ));
        }
        Ok(inspection.frames)
    }

    pub fn append_epoch(
        &mut self,
        frames: &[Arc<CommitFrame>],
    ) -> Result<(), FileEpochStoreError> {
        self.ensure_healthy()?;
        if frames.is_empty() {
            return Ok(());
        }
        self.validate_epoch(frames)?;
        let records = frames
            .iter()
            .map(|frame| encode_record(frame))
            .collect::<Result<Vec<_>, _>>()?;
        let start_offset = self
            .io
            .len(EpochIoPhase::EpochStart)
            .map_err(|source| FileEpochStoreError::Io {
                phase: EpochIoPhase::EpochStart,
                source,
            })?;
        let checkpoint = self.checkpoint(start_offset);
        self.metrics.epoch_attempts += 1;

        let result = if let Some((transport_metrics, result)) =
            self.io.persist_epoch(start_offset, &records)
        {
            self.metrics.record_persist(transport_metrics);
            result
        } else {
            match self.policy {
                FileEpochSyncPolicy::PerFrame => self.write_per_frame(start_offset, &records),
                FileEpochSyncPolicy::PerEpoch => self.write_one_epoch(start_offset, &records),
            }
        };

        if let Err((phase, source)) = result {
            return Err(self.handle_primary_failure(checkpoint, phase, source));
        }

        for record in &records {
            self.prefix_digest = digest_extend(self.prefix_digest, record);
        }
        self.frames.extend(frames.iter().cloned());
        self.recovered_tail_bytes = 0;
        self.metrics.epochs_committed += 1;
        self.metrics.frames_committed += frames.len() as u64;
        Ok(())
    }

    fn ensure_healthy(&self) -> Result<(), FileEpochStoreError> {
        match self.state {
            FileEpochState::Healthy => Ok(()),
            FileEpochState::Repairing => Err(FileEpochStoreError::StorePoisoned(
                "store is still repairing".to_owned(),
            )),
            FileEpochState::Poisoned => Err(FileEpochStoreError::StorePoisoned(
                self.poison_reason
                    .clone()
                    .unwrap_or_else(|| "repair could not be verified".to_owned()),
            )),
        }
    }

    fn checkpoint(&self, start_offset: u64) -> EpochCheckpoint {
        let (world_id, world_version) = self
            .frames
            .last()
            .map(|frame| (frame.resulting_world(), frame.resulting_version()))
            .unwrap_or((WorldId::GENESIS, 0));
        EpochCheckpoint {
            start_offset,
            frame_count: self.frames.len(),
            world_id,
            world_version,
            prefix_digest: self.prefix_digest,
        }
    }

    fn validate_epoch(&self, frames: &[Arc<CommitFrame>]) -> Result<(), FileEpochStoreError> {
        let (mut expected_parent, mut expected_version) = self
            .frames
            .last()
            .map(|frame| (frame.resulting_world(), frame.resulting_version()))
            .unwrap_or((WorldId::GENESIS, 0));

        for frame in frames {
            if frame.parent_world() != expected_parent {
                return Err(FileCommitStoreError::NonLinearAppend(format!(
                    "expected parent {expected_parent}, found {}",
                    frame.parent_world()
                ))
                .into());
            }
            if frame.parent_version() != expected_version {
                return Err(FileCommitStoreError::NonLinearAppend(format!(
                    "expected parent version {expected_version}, found {}",
                    frame.parent_version()
                ))
                .into());
            }
            let resulting = expected_version.checked_add(1).ok_or_else(|| {
                FileEpochStoreError::File(FileCommitStoreError::NonLinearAppend(
                    "world version overflow".to_owned(),
                ))
            })?;
            if frame.resulting_version() != resulting {
                return Err(FileCommitStoreError::NonLinearAppend(format!(
                    "expected resulting version {resulting}, found {}",
                    frame.resulting_version()
                ))
                .into());
            }
            expected_parent = frame.resulting_world();
            expected_version = frame.resulting_version();
        }
        Ok(())
    }

    fn write_one_epoch(
        &mut self,
        start_offset: u64,
        records: &[Vec<u8>],
    ) -> Result<(), (EpochIoPhase, std::io::Error)> {
        let total = records.iter().map(Vec::len).sum();
        let mut arena = Vec::with_capacity(total);
        for record in records {
            arena.extend_from_slice(record);
        }
        self.write_all_at(start_offset, &arena)?;
        self.sync_epoch()?;
        Ok(())
    }

    fn write_per_frame(
        &mut self,
        mut offset: u64,
        records: &[Vec<u8>],
    ) -> Result<(), (EpochIoPhase, std::io::Error)> {
        for record in records {
            self.write_all_at(offset, record)?;
            self.sync_epoch()?;
            offset += record.len() as u64;
        }
        Ok(())
    }

    fn write_all_at(
        &mut self,
        mut offset: u64,
        mut bytes: &[u8],
    ) -> Result<(), (EpochIoPhase, std::io::Error)> {
        while !bytes.is_empty() {
            self.metrics.data_writes += 1;
            match self.io.write_at(EpochIoPhase::EpochWrite, offset, bytes) {
                Ok(0) => {
                    return Err((
                        EpochIoPhase::EpochWrite,
                        std::io::Error::new(
                            std::io::ErrorKind::WriteZero,
                            "epoch write made no progress",
                        ),
                    ));
                }
                Ok(written) => {
                    self.metrics.bytes_written += written as u64;
                    offset += written as u64;
                    bytes = &bytes[written..];
                }
                Err(source) => return Err((EpochIoPhase::EpochWrite, source)),
            }
        }
        Ok(())
    }

    fn sync_epoch(&mut self) -> Result<(), (EpochIoPhase, std::io::Error)> {
        self.metrics.data_syncs += 1;
        self.io
            .sync_data(EpochIoPhase::EpochSync)
            .map_err(|source| (EpochIoPhase::EpochSync, source))
    }

    fn handle_primary_failure(
        &mut self,
        checkpoint: EpochCheckpoint,
        primary_phase: EpochIoPhase,
        primary: std::io::Error,
    ) -> FileEpochStoreError {
        self.state = FileEpochState::Repairing;
        match self.repair(checkpoint) {
            Ok(()) => {
                self.state = FileEpochState::Healthy;
                self.metrics.repairs_succeeded += 1;
                FileEpochStoreError::EpochRepaired {
                    phase: primary_phase,
                    source: primary,
                }
            }
            Err((repair_phase, repair)) => {
                self.state = FileEpochState::Poisoned;
                self.metrics.repairs_failed += 1;
                let reason = format!(
                    "primary {primary_phase:?}: {primary}; repair {repair_phase:?}: {repair}"
                );
                self.poison_reason = Some(reason.clone());
                FileEpochStoreError::RepairFailed {
                    primary_phase,
                    primary: primary.to_string(),
                    repair_phase,
                    repair,
                }
            }
        }
    }

    fn repair(
        &mut self,
        checkpoint: EpochCheckpoint,
    ) -> Result<(), (EpochIoPhase, String)> {
        self.io
            .set_len(EpochIoPhase::RepairTruncate, checkpoint.start_offset)
            .map_err(|error| (EpochIoPhase::RepairTruncate, error.to_string()))?;
        self.metrics.repair_syncs += 1;
        self.io
            .sync_data(EpochIoPhase::RepairSync)
            .map_err(|error| (EpochIoPhase::RepairSync, error.to_string()))?;

        let actual_len = self
            .io
            .len(EpochIoPhase::VerifyLength)
            .map_err(|error| (EpochIoPhase::VerifyLength, error.to_string()))?;
        if actual_len != checkpoint.start_offset {
            return Err((
                EpochIoPhase::VerifyLength,
                format!(
                    "expected repaired length {}, found {actual_len}",
                    checkpoint.start_offset
                ),
            ));
        }

        let bytes = self
            .io
            .read_all(EpochIoPhase::VerifyRead)
            .map_err(|error| (EpochIoPhase::VerifyRead, error.to_string()))?;
        if digest(&bytes) != checkpoint.prefix_digest {
            return Err((
                EpochIoPhase::VerifyRead,
                "repaired prefix digest does not match checkpoint".to_owned(),
            ));
        }
        let inspection = inspect_file(&bytes)
            .map_err(|error| (EpochIoPhase::VerifyRead, error.to_string()))?;
        if inspection.last_good_offset != bytes.len() {
            return Err((
                EpochIoPhase::VerifyRead,
                "repaired file still contains an incomplete tail".to_owned(),
            ));
        }
        if inspection.frames.len() != checkpoint.frame_count {
            return Err((
                EpochIoPhase::VerifyRead,
                format!(
                    "expected {} repaired frames, found {}",
                    checkpoint.frame_count,
                    inspection.frames.len()
                ),
            ));
        }
        let (actual_world, actual_version) = inspection
            .frames
            .last()
            .map(|frame| (frame.resulting_world(), frame.resulting_version()))
            .unwrap_or((WorldId::GENESIS, 0));
        if actual_world != checkpoint.world_id || actual_version != checkpoint.world_version {
            return Err((
                EpochIoPhase::VerifyRead,
                format!(
                    "expected repaired tail {} v{}, found {} v{}",
                    checkpoint.world_id,
                    checkpoint.world_version,
                    actual_world,
                    actual_version
                ),
            ));
        }
        World::reconstruct(&inspection.frames)
            .map_err(|error| (EpochIoPhase::VerifyRead, error.to_string()))?;
        Ok(())
    }
}

impl<I: EpochFileIo> CommitStore for FileEpochStore<I> {
    type Error = FileEpochStoreError;

    fn append(&mut self, frame: Arc<CommitFrame>) -> Result<(), Self::Error> {
        self.append_epoch(&[frame])
    }

    fn frames(&self) -> Vec<Arc<CommitFrame>> {
        // These are the last verified logical frames. Poisoning fences physical
        // reads and later writes, but it does not invalidate published worlds.
        self.frames.clone()
    }

    fn len(&self) -> usize {
        self.frames.len()
    }
}

fn digest(bytes: &[u8]) -> u64 {
    digest_extend(CHECKSUM_OFFSET_BASIS, bytes)
}

fn digest_extend(mut value: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        value ^= u64::from(*byte);
        value = value.wrapping_mul(CHECKSUM_PRIME);
    }
    value
}

fn inspect_file(bytes: &[u8]) -> Result<FileInspection, FileEpochStoreError> {
    if bytes.len() < FILE_HEADER_LEN {
        return Err(FileCommitStoreError::InvalidHeader("header is incomplete".to_owned()).into());
    }
    if &bytes[..8] != FILE_MAGIC {
        return Err(FileCommitStoreError::InvalidHeader(
            "magic bytes do not match FTHDB001".to_owned(),
        )
        .into());
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().expect("fixed header slice"));
    if version != FILE_VERSION {
        return Err(FileCommitStoreError::UnsupportedFormat(version).into());
    }
    let flags = u32::from_le_bytes(bytes[12..16].try_into().expect("fixed header slice"));
    if flags != FILE_FLAGS {
        return Err(FileCommitStoreError::InvalidHeader(format!(
            "unsupported header flags 0x{flags:08x}"
        ))
        .into());
    }

    let mut frames = Vec::new();
    let mut offset = FILE_HEADER_LEN;
    let mut last_good_offset = FILE_HEADER_LEN;
    while offset < bytes.len() {
        let remaining = bytes.len() - offset;
        if remaining < FRAME_PREFIX_LEN {
            break;
        }
        if &bytes[offset..offset + 4] != FRAME_MAGIC {
            return Err(corrupt(offset, "frame magic does not match FRM1").into());
        }
        let payload_len = u64::from_le_bytes(
            bytes[offset + 4..offset + 12]
                .try_into()
                .expect("fixed frame length slice"),
        );
        if payload_len > MAX_FRAME_BYTES {
            return Err(corrupt(
                offset,
                format!("declared payload length {payload_len} exceeds limit"),
            )
            .into());
        }
        let payload_len = usize::try_from(payload_len)
            .map_err(|_| FileEpochStoreError::File(corrupt(offset, "payload length does not fit")))?;
        let total_len = FRAME_PREFIX_LEN
            .checked_add(payload_len)
            .and_then(|value| value.checked_add(FRAME_TRAILER_LEN))
            .ok_or_else(|| FileEpochStoreError::File(corrupt(offset, "record length overflow")))?;
        if remaining < total_len {
            break;
        }
        let expected_checksum = u64::from_le_bytes(
            bytes[offset + 12..offset + 20]
                .try_into()
                .expect("fixed checksum slice"),
        );
        let payload_start = offset + FRAME_PREFIX_LEN;
        let payload_end = payload_start + payload_len;
        let trailer_end = payload_end + FRAME_TRAILER_LEN;
        if &bytes[payload_end..trailer_end] != FRAME_TRAILER {
            return Err(corrupt(offset, "completion trailer does not match END1").into());
        }
        let payload = &bytes[payload_start..payload_end];
        let actual_checksum = digest(payload);
        if actual_checksum != expected_checksum {
            return Err(corrupt(offset, "checksum mismatch").into());
        }
        let frame = Arc::new(
            decode_payload(payload)
                .map_err(|reason| FileEpochStoreError::File(corrupt(offset, reason)))?,
        );
        validate_decoded_position(&frames, &frame, offset)?;
        frames.push(frame);
        offset += total_len;
        last_good_offset = offset;
    }
    Ok(FileInspection {
        frames,
        last_good_offset,
    })
}

fn validate_decoded_position(
    frames: &[Arc<CommitFrame>],
    frame: &CommitFrame,
    offset: usize,
) -> Result<(), FileEpochStoreError> {
    let (expected_parent, expected_version) = frames
        .last()
        .map(|current| (current.resulting_world(), current.resulting_version()))
        .unwrap_or((WorldId::GENESIS, 0));
    if frame.parent_world() != expected_parent {
        return Err(corrupt(offset, "decoded parent world is nonlinear").into());
    }
    if frame.parent_version() != expected_version
        || frame.resulting_version() != expected_version + 1
    {
        return Err(corrupt(offset, "decoded world version is nonlinear").into());
    }
    Ok(())
}

fn corrupt(offset: usize, reason: impl Into<String>) -> FileCommitStoreError {
    FileCommitStoreError::CorruptFrame {
        offset: offset as u64,
        reason: reason.into(),
    }
}

fn encode_record(frame: &CommitFrame) -> Result<Vec<u8>, FileEpochStoreError> {
    let payload = encode_payload(frame);
    if payload.len() as u64 > MAX_FRAME_BYTES {
        return Err(FileCommitStoreError::NonLinearAppend(format!(
            "encoded frame payload is {} bytes, limit is {MAX_FRAME_BYTES}",
            payload.len()
        ))
        .into());
    }
    let mut record = Vec::with_capacity(FRAME_PREFIX_LEN + payload.len() + FRAME_TRAILER_LEN);
    record.extend_from_slice(FRAME_MAGIC);
    record.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    record.extend_from_slice(&digest(&payload).to_le_bytes());
    record.extend_from_slice(&payload);
    record.extend_from_slice(FRAME_TRAILER);
    Ok(record)
}

fn encode_payload(frame: &CommitFrame) -> Vec<u8> {
    let mut output = Vec::new();
    put_u64(&mut output, frame.parent_world().value());
    put_u64(&mut output, frame.resulting_world().value());
    put_u64(&mut output, frame.parent_version());
    put_u64(&mut output, frame.resulting_version());
    put_u64(&mut output, frame.resulting_allocator());
    put_u64(&mut output, frame.operations().len() as u64);
    for operation in frame.operations() {
        match operation {
            Operation::AllocateEntity { entity } => {
                output.push(0);
                put_u64(&mut output, entity.value());
            }
            Operation::Define { slot, fact } => {
                output.push(1);
                put_string(&mut output, slot.as_str());
                put_atom(&mut output, &fact.subject);
                put_string(&mut output, fact.predicate.as_str());
                put_atom(&mut output, &fact.object);
            }
            Operation::Forget { slot } => {
                output.push(2);
                put_string(&mut output, slot.as_str());
            }
        }
    }
    output
}

fn decode_payload(payload: &[u8]) -> Result<CommitFrame, String> {
    let mut decoder = EpochDecoder::new(payload);
    let parent_world = WorldId::new(decoder.u64()?);
    let resulting_world = WorldId::new(decoder.u64()?);
    let parent_version = decoder.u64()?;
    let resulting_version = decoder.u64()?;
    let resulting_allocator = decoder.u64()?;
    let operation_count = decoder.u64()?;
    if operation_count > MAX_OPERATIONS_PER_FRAME {
        return Err("operation count exceeds limit".to_owned());
    }
    let mut operations = Vec::with_capacity(operation_count as usize);
    for _ in 0..operation_count {
        operations.push(match decoder.byte()? {
            0 => Operation::AllocateEntity {
                entity: EntityId::new(decoder.u64()?),
            },
            1 => Operation::Define {
                slot: SlotId::new(decoder.string()?),
                fact: Fact::new(
                    decoder.atom()?,
                    Predicate::new(decoder.string()?),
                    decoder.atom()?,
                ),
            },
            2 => Operation::Forget {
                slot: SlotId::new(decoder.string()?),
            },
            tag => return Err(format!("unknown operation tag {tag}")),
        });
    }
    decoder.finish()?;
    Ok(CommitFrame {
        parent_world,
        resulting_world,
        parent_version,
        resulting_version,
        resulting_allocator,
        operations: Arc::from(operations),
    })
}

fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_string(output: &mut Vec<u8>, value: &str) {
    put_u64(output, value.len() as u64);
    output.extend_from_slice(value.as_bytes());
}

fn put_atom(output: &mut Vec<u8>, atom: &Atom) {
    match atom {
        Atom::Entity(entity) => {
            output.push(0);
            put_u64(output, entity.value());
        }
        Atom::Literal(literal) => {
            output.push(1);
            put_string(output, literal.as_str());
        }
    }
}

struct EpochDecoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> EpochDecoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn byte(&mut self) -> Result<u8, String> {
        let value = *self
            .bytes
            .get(self.offset)
            .ok_or_else(|| "unexpected end of payload".to_owned())?;
        self.offset += 1;
        Ok(value)
    }

    fn u64(&mut self) -> Result<u64, String> {
        let end = self
            .offset
            .checked_add(8)
            .ok_or_else(|| "payload offset overflow".to_owned())?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| "unexpected end of payload".to_owned())?;
        self.offset = end;
        Ok(u64::from_le_bytes(slice.try_into().expect("fixed u64 slice")))
    }

    fn string(&mut self) -> Result<String, String> {
        let length = usize::try_from(self.u64()?)
            .map_err(|_| "string length does not fit platform".to_owned())?;
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| "string offset overflow".to_owned())?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| "unexpected end of string".to_owned())?;
        self.offset = end;
        std::str::from_utf8(slice)
            .map(str::to_owned)
            .map_err(|error| format!("invalid UTF-8: {error}"))
    }

    fn atom(&mut self) -> Result<Atom, String> {
        match self.byte()? {
            0 => Ok(Atom::Entity(EntityId::new(self.u64()?))),
            1 => Ok(Atom::Literal(Literal::new(self.string()?))),
            tag => Err(format!("unknown atom tag {tag}")),
        }
    }

    fn finish(self) -> Result<(), String> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(format!(
                "payload has {} trailing bytes",
                self.bytes.len() - self.offset
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    const ENOSPC: i32 = 28;
    const EIO: i32 = 5;

    #[derive(Clone, Debug)]
    enum FaultAction {
        Fail(i32),
        WritePrefixThenFail { bytes: usize, errno: i32 },
        ApplyThenFail(i32),
        ReportLength(u64),
    }

    #[derive(Clone, Debug)]
    struct FaultRule {
        phase: EpochIoPhase,
        action: FaultAction,
    }

    struct ScriptedIo {
        inner: StdEpochFileIo,
        rules: VecDeque<FaultRule>,
    }

    impl ScriptedIo {
        fn open(path: &Path, rules: Vec<FaultRule>) -> Self {
            Self {
                inner: StdEpochFileIo::open(path).expect("scripted file opens"),
                rules: rules.into(),
            }
        }

        fn take(&mut self, phase: EpochIoPhase) -> Option<FaultAction> {
            let index = self.rules.iter().position(|rule| rule.phase == phase)?;
            self.rules.remove(index).map(|rule| rule.action)
        }
    }

    fn injected(errno: i32) -> std::io::Error {
        std::io::Error::from_raw_os_error(errno)
    }

    impl EpochFileIo for ScriptedIo {
        fn len(&mut self, phase: EpochIoPhase) -> std::io::Result<u64> {
            match self.take(phase) {
                Some(FaultAction::Fail(errno)) => Err(injected(errno)),
                Some(FaultAction::ReportLength(length)) => Ok(length),
                Some(action) => panic!("unsupported len action {action:?}"),
                None => self.inner.len(phase),
            }
        }

        fn write_at(
            &mut self,
            phase: EpochIoPhase,
            offset: u64,
            bytes: &[u8],
        ) -> std::io::Result<usize> {
            match self.take(phase) {
                Some(FaultAction::Fail(errno)) => Err(injected(errno)),
                Some(FaultAction::WritePrefixThenFail { bytes: count, errno }) => {
                    let count = count.min(bytes.len());
                    if count != 0 {
                        let written = self.inner.write_at(phase, offset, &bytes[..count])?;
                        assert_eq!(written, count);
                    }
                    Err(injected(errno))
                }
                Some(FaultAction::ApplyThenFail(errno)) => {
                    let written = self.inner.write_at(phase, offset, bytes)?;
                    assert_eq!(written, bytes.len());
                    Err(injected(errno))
                }
                Some(action) => panic!("unsupported write action {action:?}"),
                None => self.inner.write_at(phase, offset, bytes),
            }
        }

        fn sync_data(&mut self, phase: EpochIoPhase) -> std::io::Result<()> {
            match self.take(phase) {
                Some(FaultAction::Fail(errno)) => Err(injected(errno)),
                Some(FaultAction::ApplyThenFail(errno)) => {
                    self.inner.sync_data(phase)?;
                    Err(injected(errno))
                }
                Some(action) => panic!("unsupported sync action {action:?}"),
                None => self.inner.sync_data(phase),
            }
        }

        fn set_len(&mut self, phase: EpochIoPhase, len: u64) -> std::io::Result<()> {
            match self.take(phase) {
                Some(FaultAction::Fail(errno)) => Err(injected(errno)),
                Some(FaultAction::ApplyThenFail(errno)) => {
                    self.inner.set_len(phase, len)?;
                    Err(injected(errno))
                }
                Some(action) => panic!("unsupported truncate action {action:?}"),
                None => self.inner.set_len(phase, len),
            }
        }

        fn read_all(&mut self, phase: EpochIoPhase) -> std::io::Result<Vec<u8>> {
            match self.take(phase) {
                Some(FaultAction::Fail(errno)) => Err(injected(errno)),
                Some(action) => panic!("unsupported read action {action:?}"),
                None => self.inner.read_all(phase),
            }
        }
    }

    fn temp_path(label: &str) -> PathBuf {
        let sequence = PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "forthdb-file-epoch-{label}-{}-{sequence}.db",
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

    fn three_frames() -> Vec<Arc<CommitFrame>> {
        let database = Database::new(MemoryCommitStore::new()).expect("memory database opens");
        let mut first = database.begin();
        assert_eq!(first.entity(), EntityId::new(1));
        first.define(SlotId::new("state/1"), state_fact("one"));
        database.commit(first).expect("first commits");
        for index in 2..=3 {
            let mut transaction = database.begin();
            transaction.define(
                SlotId::new(format!("state/{index}")),
                state_fact(&index.to_string()),
            );
            database.commit(transaction).expect("frame commits");
        }
        database.frames()
    }

    fn initialized(path: &Path) {
        let store = FileCommitStore::open(path).expect("file initializes");
        assert_eq!(store.len(), 0);
    }

    fn scripted_store(
        path: &Path,
        policy: FileEpochSyncPolicy,
        rules: Vec<FaultRule>,
    ) -> FileEpochStore<ScriptedIo> {
        initialized(path);
        FileEpochStore::from_io(path, ScriptedIo::open(path, rules), policy)
            .expect("scripted store opens")
    }

    #[test]
    fn epoch_and_per_frame_policies_produce_identical_v1_bytes() {
        let frames = three_frames();
        let sequential_path = temp_path("sequential");
        let epoch_path = temp_path("epoch");
        let mut sequential =
            FileEpochStore::open(&sequential_path, FileEpochSyncPolicy::PerFrame)
                .expect("sequential opens");
        let mut epoch = FileEpochStore::open(&epoch_path, FileEpochSyncPolicy::PerEpoch)
            .expect("epoch opens");
        sequential.append_epoch(&frames).expect("sequential commits");
        epoch.append_epoch(&frames).expect("epoch commits");
        assert_eq!(fs::read(&sequential_path).unwrap(), fs::read(&epoch_path).unwrap());
        assert_eq!(sequential.metrics().data_syncs, 3);
        assert_eq!(epoch.metrics().data_syncs, 1);
        let _ = fs::remove_file(sequential_path);
        let _ = fs::remove_file(epoch_path);
    }

    #[test]
    fn prefix_write_enospc_repairs_exact_checkpoint_and_store_remains_reusable() {
        let path = temp_path("prefix-repair");
        let frames = three_frames();
        let original = fs::read({ initialized(&path); &path }).expect("prefix reads");
        let rules = vec![FaultRule {
            phase: EpochIoPhase::EpochWrite,
            action: FaultAction::WritePrefixThenFail {
                bytes: 37,
                errno: ENOSPC,
            },
        }];
        let mut store = FileEpochStore::from_io(
            &path,
            ScriptedIo::open(&path, rules),
            FileEpochSyncPolicy::PerEpoch,
        )
        .expect("store opens");
        assert!(matches!(
            store.append_epoch(&frames),
            Err(FileEpochStoreError::EpochRepaired { .. })
        ));
        assert_eq!(store.state(), FileEpochState::Healthy);
        assert_eq!(store.len(), 0);
        assert_eq!(fs::read(&path).unwrap(), original);
        store.append_epoch(&frames).expect("next epoch succeeds");
        assert_eq!(store.len(), 3);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn epoch_sync_eio_repairs_even_after_full_arena_write() {
        let path = temp_path("sync-repair");
        let frames = three_frames();
        let mut store = scripted_store(
            &path,
            FileEpochSyncPolicy::PerEpoch,
            vec![FaultRule {
                phase: EpochIoPhase::EpochSync,
                action: FaultAction::Fail(EIO),
            }],
        );
        assert!(matches!(
            store.append_epoch(&frames),
            Err(FileEpochStoreError::EpochRepaired {
                phase: EpochIoPhase::EpochSync,
                ..
            })
        ));
        assert_eq!(store.state(), FileEpochState::Healthy);
        assert_eq!(store.len(), 0);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn truncate_failure_poisoning_is_permanent_for_live_handle() {
        let path = temp_path("truncate-poison");
        let frames = three_frames();
        let mut store = scripted_store(
            &path,
            FileEpochSyncPolicy::PerEpoch,
            vec![
                FaultRule {
                    phase: EpochIoPhase::EpochWrite,
                    action: FaultAction::WritePrefixThenFail {
                        bytes: 19,
                        errno: ENOSPC,
                    },
                },
                FaultRule {
                    phase: EpochIoPhase::RepairTruncate,
                    action: FaultAction::Fail(EIO),
                },
            ],
        );
        assert!(matches!(
            store.append_epoch(&frames),
            Err(FileEpochStoreError::RepairFailed { .. })
        ));
        assert_eq!(store.state(), FileEpochState::Poisoned);
        assert!(matches!(
            store.append_epoch(&frames),
            Err(FileEpochStoreError::StorePoisoned(_))
        ));
        assert!(matches!(
            store.physical_frames(),
            Err(FileEpochStoreError::StorePoisoned(_))
        ));
        assert_eq!(store.frames().len(), 0);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn truncate_that_applies_but_reports_eio_still_poisoned() {
        let path = temp_path("truncate-uncertain");
        let frames = three_frames();
        let mut store = scripted_store(
            &path,
            FileEpochSyncPolicy::PerEpoch,
            vec![
                FaultRule {
                    phase: EpochIoPhase::EpochWrite,
                    action: FaultAction::Fail(ENOSPC),
                },
                FaultRule {
                    phase: EpochIoPhase::RepairTruncate,
                    action: FaultAction::ApplyThenFail(EIO),
                },
            ],
        );
        assert!(matches!(
            store.append_epoch(&frames),
            Err(FileEpochStoreError::RepairFailed { .. })
        ));
        assert_eq!(store.state(), FileEpochState::Poisoned);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn repair_sync_failure_poisoned() {
        let path = temp_path("repair-sync-poison");
        let frames = three_frames();
        let mut store = scripted_store(
            &path,
            FileEpochSyncPolicy::PerEpoch,
            vec![
                FaultRule {
                    phase: EpochIoPhase::EpochWrite,
                    action: FaultAction::Fail(ENOSPC),
                },
                FaultRule {
                    phase: EpochIoPhase::RepairSync,
                    action: FaultAction::Fail(EIO),
                },
            ],
        );
        assert!(matches!(
            store.append_epoch(&frames),
            Err(FileEpochStoreError::RepairFailed { .. })
        ));
        assert_eq!(store.state(), FileEpochState::Poisoned);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn verification_length_mismatch_poisoned() {
        let path = temp_path("length-poison");
        let frames = three_frames();
        let mut store = scripted_store(
            &path,
            FileEpochSyncPolicy::PerEpoch,
            vec![
                FaultRule {
                    phase: EpochIoPhase::EpochWrite,
                    action: FaultAction::Fail(ENOSPC),
                },
                FaultRule {
                    phase: EpochIoPhase::VerifyLength,
                    action: FaultAction::ReportLength(999),
                },
            ],
        );
        assert!(matches!(
            store.append_epoch(&frames),
            Err(FileEpochStoreError::RepairFailed { .. })
        ));
        assert_eq!(store.state(), FileEpochState::Poisoned);
        let _ = fs::remove_file(path);
    }
}
