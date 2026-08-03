use super::file_store::{FileCommitStore, FileCommitStoreError};
use super::*;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use io_uring::{opcode, squeue, types, IoUring};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

const FILE_HEADER_LEN: usize = 16;
const FRAME_MAGIC: &[u8; 4] = b"FRM1";
const FRAME_TRAILER: &[u8; 4] = b"END1";
const FRAME_PREFIX_LEN: usize = 20;
const FRAME_TRAILER_LEN: usize = 4;
const MAX_FRAME_BYTES: u64 = 64 * 1024 * 1024;
const CHECKSUM_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const CHECKSUM_PRIME: u64 = 0x100000001b3;

#[cfg(target_os = "linux")]
const RING_ENTRIES: u32 = 2;
#[cfg(target_os = "linux")]
const WRITE_USER_DATA: u64 = 0x4654_4844_4257_5254;
#[cfg(target_os = "linux")]
const FSYNC_USER_DATA: u64 = 0x4654_4844_4246_5359;

#[derive(Debug)]
pub enum IoUringCommitStoreError {
    UnsupportedPlatform,
    Recovery(FileCommitStoreError),
    Io(std::io::Error),
    QueueFull,
    MissingCompletion(&'static str),
    OperationFailed {
        operation: &'static str,
        error: std::io::Error,
    },
    UnexpectedCompletion {
        operation: &'static str,
        result: i32,
    },
    ShortWrite {
        expected: usize,
        actual: usize,
    },
    NonLinearAppend(String),
    FrameTooLarge(usize),
}

impl fmt::Display for IoUringCommitStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                write!(formatter, "IoUringCommitStore is supported only on Linux")
            }
            Self::Recovery(error) => write!(formatter, "io_uring store recovery failed: {error}"),
            Self::Io(error) => write!(formatter, "io_uring store I/O failed: {error}"),
            Self::QueueFull => write!(formatter, "io_uring submission queue could not accept a commit"),
            Self::MissingCompletion(operation) => {
                write!(formatter, "io_uring did not return the {operation} completion")
            }
            Self::OperationFailed { operation, error } => {
                write!(formatter, "io_uring {operation} failed: {error}")
            }
            Self::UnexpectedCompletion { operation, result } => {
                write!(formatter, "io_uring {operation} returned unexpected result {result}")
            }
            Self::ShortWrite { expected, actual } => write!(
                formatter,
                "io_uring write completed only {actual} of {expected} bytes"
            ),
            Self::NonLinearAppend(message) => write!(formatter, "nonlinear commit append: {message}"),
            Self::FrameTooLarge(length) => write!(
                formatter,
                "encoded frame record is {length} bytes and cannot be submitted as one io_uring write"
            ),
        }
    }
}

impl Error for IoUringCommitStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Recovery(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::OperationFailed { error, .. } => Some(error),
            Self::UnsupportedPlatform
            | Self::QueueFull
            | Self::MissingCompletion(_)
            | Self::UnexpectedCompletion { .. }
            | Self::ShortWrite { .. }
            | Self::NonLinearAppend(_)
            | Self::FrameTooLarge(_) => None,
        }
    }
}

impl From<std::io::Error> for IoUringCommitStoreError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<FileCommitStoreError> for IoUringCommitStoreError {
    fn from(value: FileCommitStoreError) -> Self {
        Self::Recovery(value)
    }
}

#[cfg(target_os = "linux")]
pub struct IoUringCommitStore {
    path: PathBuf,
    file: File,
    ring: IoUring,
    frames: Vec<Arc<CommitFrame>>,
    recovered_tail_bytes: u64,
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub struct IoUringCommitStore {
    path: PathBuf,
}

#[cfg(target_os = "linux")]
impl fmt::Debug for IoUringCommitStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IoUringCommitStore")
            .field("path", &self.path)
            .field("frames", &self.frames.len())
            .field("recovered_tail_bytes", &self.recovered_tail_bytes)
            .field("ring_entries", &RING_ENTRIES)
            .field("max_in_flight_commits", &1)
            .finish()
    }
}

impl IoUringCommitStore {
    #[cfg(target_os = "linux")]
    pub fn open(path: impl AsRef<Path>) -> Result<Self, IoUringCommitStoreError> {
        let path = path.as_ref().to_path_buf();

        // Reuse the established ordinary-I/O recovery implementation so both
        // durable stores accept exactly the same version 1 history and tail rules.
        let recovered = FileCommitStore::open(&path)?;
        let recovered_tail_bytes = recovered.recovered_tail_bytes();
        let frames = recovered.frames();
        drop(recovered);

        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        file.seek(SeekFrom::End(0))?;
        let ring = IoUring::new(RING_ENTRIES)?;

        Ok(Self {
            path,
            file,
            ring,
            frames,
            recovered_tail_bytes,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn open(path: impl AsRef<Path>) -> Result<Self, IoUringCommitStoreError> {
        let _ = path.as_ref();
        Err(IoUringCommitStoreError::UnsupportedPlatform)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(target_os = "linux")]
    pub fn recovered_tail_bytes(&self) -> u64 {
        self.recovered_tail_bytes
    }

    #[cfg(not(target_os = "linux"))]
    pub fn recovered_tail_bytes(&self) -> u64 {
        0
    }

    #[cfg(target_os = "linux")]
    pub fn file_len(&self) -> Result<u64, std::io::Error> {
        self.file.metadata().map(|metadata| metadata.len())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn file_len(&self) -> Result<u64, std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "IoUringCommitStore is supported only on Linux",
        ))
    }

    pub const fn max_in_flight_commits(&self) -> usize {
        1
    }

    #[cfg(target_os = "linux")]
    pub const fn ring_entries(&self) -> u32 {
        RING_ENTRIES
    }

    #[cfg(not(target_os = "linux"))]
    pub const fn ring_entries(&self) -> u32 {
        0
    }

    #[cfg(target_os = "linux")]
    fn validate_append(&self, frame: &CommitFrame) -> Result<(), IoUringCommitStoreError> {
        let (expected_parent, expected_parent_version) = self
            .frames
            .last()
            .map(|current| (current.resulting_world(), current.resulting_version()))
            .unwrap_or((WorldId::GENESIS, 0));

        if frame.parent_world() != expected_parent {
            return Err(IoUringCommitStoreError::NonLinearAppend(format!(
                "expected parent {expected_parent}, found {}",
                frame.parent_world()
            )));
        }
        if frame.parent_version() != expected_parent_version {
            return Err(IoUringCommitStoreError::NonLinearAppend(format!(
                "expected parent version {expected_parent_version}, found {}",
                frame.parent_version()
            )));
        }
        let expected_resulting_version = expected_parent_version.checked_add(1).ok_or_else(|| {
            IoUringCommitStoreError::NonLinearAppend("world version overflow".to_owned())
        })?;
        if frame.resulting_version() != expected_resulting_version {
            return Err(IoUringCommitStoreError::NonLinearAppend(format!(
                "expected resulting version {expected_resulting_version}, found {}",
                frame.resulting_version()
            )));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn reset_ring(&mut self) -> Result<(), std::io::Error> {
        let replacement = IoUring::new(RING_ENTRIES)?;
        let previous = std::mem::replace(&mut self.ring, replacement);
        drop(previous);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn rollback_failed_append(&mut self, start: u64) {
        let _ = self.file.set_len(start);
        let _ = self.file.seek(SeekFrom::End(0));
        let _ = self.file.sync_data();
    }

    #[cfg(target_os = "linux")]
    fn submit_durable_record(
        &mut self,
        start: u64,
        record: &[u8],
    ) -> Result<(), IoUringCommitStoreError> {
        let length = u32::try_from(record.len())
            .map_err(|_| IoUringCommitStoreError::FrameTooLarge(record.len()))?;
        let descriptor = types::Fd(self.file.as_raw_fd());

        let write = opcode::Write::new(descriptor, record.as_ptr(), length)
            .offset(start)
            .build()
            .flags(squeue::Flags::IO_LINK)
            .user_data(WRITE_USER_DATA);
        let fsync = opcode::Fsync::new(descriptor)
            .flags(types::FsyncFlags::DATASYNC)
            .build()
            .user_data(FSYNC_USER_DATA);
        let entries = [write, fsync];

        {
            let mut submission = self.ring.submission();
            // SAFETY: `record` and `self.file` remain alive until both linked
            // operations complete below, and the ring is used by this store alone.
            unsafe {
                submission
                    .push_multiple(&entries)
                    .map_err(|_| IoUringCommitStoreError::QueueFull)?;
            }
        }

        if let Err(error) = self.ring.submit_and_wait(2) {
            // Drop the ring before truncating so no request can still reference
            // the record buffer or race the rollback attempt.
            let reset_error = self.reset_ring().err();
            self.rollback_failed_append(start);
            return Err(IoUringCommitStoreError::Io(reset_error.unwrap_or(error)));
        }

        let mut write_result = None;
        let mut fsync_result = None;
        {
            let mut completion = self.ring.completion();
            while let Some(entry) = completion.next() {
                match entry.user_data() {
                    WRITE_USER_DATA => write_result = Some(entry.result()),
                    FSYNC_USER_DATA => fsync_result = Some(entry.result()),
                    _ => {}
                }
            }
        }

        let write_result = write_result
            .ok_or(IoUringCommitStoreError::MissingCompletion("write"))?;
        let fsync_result = fsync_result
            .ok_or(IoUringCommitStoreError::MissingCompletion("data synchronization"))?;

        if write_result < 0 {
            self.rollback_failed_append(start);
            return Err(IoUringCommitStoreError::OperationFailed {
                operation: "write",
                error: std::io::Error::from_raw_os_error(-write_result),
            });
        }
        if write_result as usize != record.len() {
            self.rollback_failed_append(start);
            return Err(IoUringCommitStoreError::ShortWrite {
                expected: record.len(),
                actual: write_result as usize,
            });
        }
        if fsync_result < 0 {
            self.rollback_failed_append(start);
            return Err(IoUringCommitStoreError::OperationFailed {
                operation: "data synchronization",
                error: std::io::Error::from_raw_os_error(-fsync_result),
            });
        }
        if fsync_result != 0 {
            self.rollback_failed_append(start);
            return Err(IoUringCommitStoreError::UnexpectedCompletion {
                operation: "data synchronization",
                result: fsync_result,
            });
        }

        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl CommitStore for IoUringCommitStore {
    type Error = IoUringCommitStoreError;

    fn append(&mut self, frame: Arc<CommitFrame>) -> Result<(), Self::Error> {
        self.validate_append(&frame)?;
        let record = encode_record(&frame)?;
        let start = self.file.metadata()?.len();
        self.submit_durable_record(start, &record)?;
        self.frames.push(frame);
        self.recovered_tail_bytes = 0;
        Ok(())
    }

    fn frames(&self) -> Vec<Arc<CommitFrame>> {
        self.frames.clone()
    }

    fn len(&self) -> usize {
        self.frames.len()
    }
}

#[cfg(not(target_os = "linux"))]
impl CommitStore for IoUringCommitStore {
    type Error = IoUringCommitStoreError;

    fn append(&mut self, _frame: Arc<CommitFrame>) -> Result<(), Self::Error> {
        Err(IoUringCommitStoreError::UnsupportedPlatform)
    }

    fn frames(&self) -> Vec<Arc<CommitFrame>> {
        Vec::new()
    }
}

fn encode_record(frame: &CommitFrame) -> Result<Vec<u8>, IoUringCommitStoreError> {
    let payload = encode_payload(frame);
    if payload.len() as u64 > MAX_FRAME_BYTES {
        return Err(IoUringCommitStoreError::FrameTooLarge(payload.len()));
    }

    let mut record = Vec::with_capacity(FRAME_PREFIX_LEN + payload.len() + FRAME_TRAILER_LEN);
    record.extend_from_slice(FRAME_MAGIC);
    record.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    record.extend_from_slice(&checksum(&payload).to_le_bytes());
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

fn checksum(bytes: &[u8]) -> u64 {
    let mut value = CHECKSUM_OFFSET_BASIS;
    for byte in bytes {
        value ^= u64::from(*byte);
        value = value.wrapping_mul(CHECKSUM_PRIME);
    }
    value
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "forthdb-io-uring-{name}-{}-{sequence}.log",
                std::process::id()
            ));
            let _ = fs::remove_file(&path);
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn state_fact(entity: EntityId, value: &str) -> Fact {
        Fact::new(
            Atom::Entity(entity),
            Predicate::new("state"),
            Atom::Literal(Literal::new(value)),
        )
    }

    fn open_or_skip(path: &Path) -> Option<IoUringCommitStore> {
        match IoUringCommitStore::open(path) {
            Ok(store) => Some(store),
            Err(IoUringCommitStoreError::Io(error))
                if matches!(error.raw_os_error(), Some(1 | 38 | 95)) =>
            {
                eprintln!("io_uring unavailable on this kernel or runner: {error}");
                None
            }
            Err(error) => panic!("io_uring store should open: {error}"),
        }
    }

    fn commit_one_world<S: CommitStore>(database: &Database<S>) -> WorldId {
        let mut transaction = database.begin();
        let entity = transaction.entity();
        transaction.define(
            SlotId::new("durable/state"),
            state_fact(entity, "ready"),
        );
        database
            .commit(transaction)
            .expect("durable commit succeeds")
            .id()
    }

    #[test]
    fn io_uring_commit_reopens_through_file_store() {
        let temp = TempFile::new("reopen-file");
        let Some(store) = open_or_skip(temp.path()) else {
            return;
        };
        assert_eq!(store.ring_entries(), 2);
        assert_eq!(store.max_in_flight_commits(), 1);
        let database = Database::new(store).expect("empty history is valid");
        let committed = commit_one_world(&database);
        drop(database);

        let reopened = FileCommitStore::open(temp.path()).expect("file store reopens history");
        let database = Database::new(reopened).expect("history reconstructs");
        assert_eq!(database.snapshot().id(), committed);
        assert_eq!(database.snapshot().version(), 1);
    }

    #[test]
    fn file_store_history_can_continue_through_io_uring() {
        let temp = TempFile::new("file-to-ring");
        let file_database = Database::new(
            FileCommitStore::open(temp.path()).expect("file store opens"),
        )
        .expect("empty history is valid");
        commit_one_world(&file_database);
        drop(file_database);

        let Some(store) = open_or_skip(temp.path()) else {
            return;
        };
        let database = Database::new(store).expect("file history reconstructs through ring");
        database
            .commit(database.begin())
            .expect("io_uring continuation commits");
        assert_eq!(database.snapshot().version(), 2);
        drop(database);

        let reopened = Database::new(
            FileCommitStore::open(temp.path()).expect("file store reopens combined history"),
        )
        .expect("combined history reconstructs");
        assert_eq!(reopened.snapshot().version(), 2);
    }

    #[test]
    fn io_uring_and_file_store_emit_identical_v1_bytes() {
        let file_temp = TempFile::new("canonical-file");
        let ring_temp = TempFile::new("canonical-ring");

        let file_database = Database::new(
            FileCommitStore::open(file_temp.path()).expect("file store opens"),
        )
        .expect("empty history is valid");
        commit_one_world(&file_database);
        drop(file_database);

        let Some(ring_store) = open_or_skip(ring_temp.path()) else {
            return;
        };
        let ring_database = Database::new(ring_store).expect("empty history is valid");
        commit_one_world(&ring_database);
        drop(ring_database);

        assert_eq!(
            fs::read(file_temp.path()).expect("read ordinary file bytes"),
            fs::read(ring_temp.path()).expect("read io_uring file bytes")
        );
    }

    #[test]
    fn incomplete_tail_recovery_matches_file_store() {
        let temp = TempFile::new("tail");
        let Some(store) = open_or_skip(temp.path()) else {
            return;
        };
        let database = Database::new(store).expect("empty history is valid");
        commit_one_world(&database);
        drop(database);

        let clean_len = fs::metadata(temp.path()).expect("metadata exists").len();
        let mut file = OpenOptions::new()
            .append(true)
            .open(temp.path())
            .expect("append incomplete tail");
        use std::io::Write;
        file.write_all(b"FRM1\x10\x00\x00")
            .expect("write incomplete tail");
        file.sync_data().expect("sync incomplete tail");
        drop(file);

        let Some(recovered) = open_or_skip(temp.path()) else {
            return;
        };
        assert_eq!(recovered.recovered_tail_bytes(), 7);
        assert_eq!(recovered.file_len().expect("file length"), clean_len);
        assert_eq!(recovered.len(), 1);
    }

    #[test]
    fn encoded_record_retains_v1_layout() {
        let temp = TempFile::new("layout");
        let memory = Database::new(MemoryCommitStore::new()).expect("memory store opens");
        commit_one_world(&memory);
        let frame = memory.frames().pop().expect("one frame exists");
        let record = encode_record(&frame).expect("record encodes");

        assert_eq!(&record[..4], FRAME_MAGIC);
        let payload_len = u64::from_le_bytes(record[4..12].try_into().unwrap()) as usize;
        assert_eq!(record.len(), FRAME_PREFIX_LEN + payload_len + FRAME_TRAILER_LEN);
        assert_eq!(&record[record.len() - 4..], FRAME_TRAILER);
        assert!(record.len() > FILE_HEADER_LEN);
        drop(temp);
    }
}
