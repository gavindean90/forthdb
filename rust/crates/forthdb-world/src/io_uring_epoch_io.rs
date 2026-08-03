use super::*;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use io_uring::{IoUring, opcode, squeue, types};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

const DEFAULT_RING_ENTRIES: u32 = 64;
const WRITE_USER_DATA_TAG: u64 = 0x5752_4954_0000_0000;
const WRITE_USER_DATA_MASK: u64 = 0xffff_ffff_0000_0000;
const FSYNC_USER_DATA: u64 = 0x4653_594e_435f_4d36;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IoUringEpochStrategy {
    ContiguousWrite,
    VectoredWrite,
    PipelinedWrites,
}

impl IoUringEpochStrategy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContiguousWrite => "contiguous_write",
            Self::VectoredWrite => "writev",
            Self::PipelinedWrites => "pipelined_writes",
        }
    }
}

#[cfg(target_os = "linux")]
pub struct IoUringEpochFileIo {
    path: PathBuf,
    file: File,
    ring: Option<IoUring>,
    ring_entries: u32,
    strategy: IoUringEpochStrategy,
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub struct IoUringEpochFileIo {
    path: PathBuf,
    strategy: IoUringEpochStrategy,
}

#[cfg(target_os = "linux")]
impl std::fmt::Debug for IoUringEpochFileIo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IoUringEpochFileIo")
            .field("path", &self.path)
            .field("strategy", &self.strategy)
            .field("ring_entries", &self.ring_entries)
            .field("ring_alive", &self.ring.is_some())
            .finish()
    }
}

pub type IoUringEpochStore = FileEpochStore<IoUringEpochFileIo>;

impl IoUringEpochFileIo {
    pub fn open_store(
        path: impl AsRef<Path>,
        strategy: IoUringEpochStrategy,
    ) -> Result<IoUringEpochStore, FileEpochStoreError> {
        Self::open_store_with_entries(path, strategy, DEFAULT_RING_ENTRIES)
    }

    #[cfg(target_os = "linux")]
    pub fn open_store_with_entries(
        path: impl AsRef<Path>,
        strategy: IoUringEpochStrategy,
        ring_entries: u32,
    ) -> Result<IoUringEpochStore, FileEpochStoreError> {
        if ring_entries < 2 {
            return Err(FileEpochStoreError::Io {
                phase: EpochIoPhase::VerifyRead,
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "io_uring epoch transport requires at least two ring entries",
                ),
            });
        }
        let path = path.as_ref().to_path_buf();
        // Reuse the established cold-open recovery and header initialization.
        let cold = FileCommitStore::open(&path)?;
        drop(cold);
        let io = Self::open(&path, strategy, ring_entries).map_err(|source| {
            FileEpochStoreError::Io {
                phase: EpochIoPhase::VerifyRead,
                source,
            }
        })?;
        FileEpochStore::from_io(path, io, FileEpochSyncPolicy::PerEpoch)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn open_store_with_entries(
        path: impl AsRef<Path>,
        strategy: IoUringEpochStrategy,
        _ring_entries: u32,
    ) -> Result<IoUringEpochStore, FileEpochStoreError> {
        let _ = path.as_ref();
        let _ = strategy;
        Err(FileEpochStoreError::Io {
            phase: EpochIoPhase::VerifyRead,
            source: std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "io_uring epoch transport is supported only on Linux",
            ),
        })
    }

    pub const fn strategy(&self) -> IoUringEpochStrategy {
        self.strategy
    }

    #[cfg(target_os = "linux")]
    pub const fn ring_entries(&self) -> u32 {
        self.ring_entries
    }

    #[cfg(not(target_os = "linux"))]
    pub const fn ring_entries(&self) -> u32 {
        0
    }

    #[cfg(target_os = "linux")]
    fn open(
        path: impl AsRef<Path>,
        strategy: IoUringEpochStrategy,
        ring_entries: u32,
    ) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        let ring = IoUring::new(ring_entries)?;
        Ok(Self {
            path,
            file,
            ring: Some(ring),
            ring_entries,
            strategy,
        })
    }

    #[cfg(target_os = "linux")]
    fn descriptor(&self) -> types::Fd {
        types::Fd(self.file.as_raw_fd())
    }

    #[cfg(target_os = "linux")]
    fn discard_and_recreate_ring(&mut self) -> std::io::Result<()> {
        drop(self.ring.take());
        self.ring = Some(IoUring::new(self.ring_entries)?);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn require_ring(&mut self) -> std::io::Result<&mut IoUring> {
        self.ring.as_mut().ok_or_else(|| {
            std::io::Error::other("io_uring epoch transport is unavailable after ring failure")
        })
    }

    #[cfg(target_os = "linux")]
    fn submit_entries(
        &mut self,
        entries: &[squeue::Entry],
        expected_writes: &[(u64, usize)],
        expected_cqes: usize,
        metrics: &mut EpochPersistMetrics,
    ) -> Result<(), (EpochIoPhase, std::io::Error)> {
        if entries.len() > self.ring_entries as usize {
            return Err((
                EpochIoPhase::EpochWrite,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "io_uring epoch requires {} SQEs but ring has {} entries",
                        entries.len(),
                        self.ring_entries
                    ),
                ),
            ));
        }

        let ring = self
            .require_ring()
            .map_err(|error| (EpochIoPhase::EpochWrite, error))?;
        {
            let mut submission = ring.submission();
            // SAFETY: every record buffer, optional arena, and iovec table used
            // by these entries remains alive in the caller until this function
            // waits for and validates every expected CQE.
            unsafe {
                submission.push_multiple(entries).map_err(|_| {
                    (
                        EpochIoPhase::EpochWrite,
                        std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            "io_uring submission queue could not accept the complete epoch",
                        ),
                    )
                })?;
            }
        }
        metrics.submission_calls += 1;

        if let Err(error) = ring.submit_and_wait(expected_cqes) {
            let reset = self.discard_and_recreate_ring().err();
            return Err((EpochIoPhase::EpochWrite, reset.unwrap_or(error)));
        }

        let mut completions = Vec::with_capacity(expected_cqes);
        {
            let ring = self
                .require_ring()
                .map_err(|error| (EpochIoPhase::EpochWrite, error))?;
            let mut completion = ring.completion();
            while let Some(entry) = completion.next() {
                completions.push((entry.user_data(), entry.result()));
            }
        }
        metrics.completion_events += completions.len() as u64;

        if let Err(error) = validate_completion_batch(expected_writes, expected_cqes, &completions)
        {
            let reset = self.discard_and_recreate_ring().err();
            return Err(reset
                .map(|source| (EpochIoPhase::EpochWrite, source))
                .unwrap_or(error));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn persist_contiguous(
        &mut self,
        start_offset: u64,
        records: &[Vec<u8>],
        metrics: &mut EpochPersistMetrics,
    ) -> Result<(), (EpochIoPhase, std::io::Error)> {
        let total = records.iter().map(Vec::len).sum::<usize>();
        let length = u32::try_from(total).map_err(|_| {
            (
                EpochIoPhase::EpochWrite,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "contiguous epoch exceeds one io_uring write length",
                ),
            )
        })?;
        let mut arena = Vec::with_capacity(total);
        for record in records {
            arena.extend_from_slice(record);
        }
        metrics.data_writes = 1;
        metrics.data_syncs = 1;
        metrics.bytes_written = total as u64;
        metrics.maximum_in_flight_writes = 1;
        metrics.arena_bytes_copied = total as u64;

        let descriptor = self.descriptor();
        let write = opcode::Write::new(descriptor, arena.as_ptr(), length)
            .offset(start_offset)
            .build()
            .flags(squeue::Flags::IO_LINK)
            .user_data(write_user_data(0));
        let fsync = opcode::Fsync::new(descriptor)
            .flags(types::FsyncFlags::DATASYNC)
            .build()
            .user_data(FSYNC_USER_DATA);
        let entries = [write, fsync];
        self.submit_entries(
            &entries,
            &[(write_user_data(0), total)],
            entries.len(),
            metrics,
        )
    }

    #[cfg(target_os = "linux")]
    fn persist_vectored(
        &mut self,
        start_offset: u64,
        records: &[Vec<u8>],
        metrics: &mut EpochPersistMetrics,
    ) -> Result<(), (EpochIoPhase, std::io::Error)> {
        let maximum = unsafe { libc::sysconf(libc::_SC_IOV_MAX) };
        let maximum = if maximum > 0 { maximum as usize } else { 1024 };
        if records.len() > maximum {
            return Err((
                EpochIoPhase::EpochWrite,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "WRITEV epoch has {} records, exceeding IOV_MAX {maximum}",
                        records.len()
                    ),
                ),
            ));
        }
        let total = records.iter().map(Vec::len).sum::<usize>();
        let iovec_count = u32::try_from(records.len()).map_err(|_| {
            (
                EpochIoPhase::EpochWrite,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "WRITEV iovec count exceeds u32",
                ),
            )
        })?;
        let iovecs = records
            .iter()
            .map(|record| libc::iovec {
                iov_base: record.as_ptr().cast_mut().cast(),
                iov_len: record.len(),
            })
            .collect::<Vec<_>>();
        metrics.data_writes = 1;
        metrics.data_syncs = 1;
        metrics.bytes_written = total as u64;
        metrics.maximum_in_flight_writes = 1;
        metrics.iovecs_submitted = records.len() as u64;

        let descriptor = self.descriptor();
        let write = opcode::Writev::new(descriptor, iovecs.as_ptr(), iovec_count)
            .offset(start_offset)
            .build()
            .flags(squeue::Flags::IO_LINK)
            .user_data(write_user_data(0));
        let fsync = opcode::Fsync::new(descriptor)
            .flags(types::FsyncFlags::DATASYNC)
            .build()
            .user_data(FSYNC_USER_DATA);
        let entries = [write, fsync];
        self.submit_entries(
            &entries,
            &[(write_user_data(0), total)],
            entries.len(),
            metrics,
        )
    }

    #[cfg(target_os = "linux")]
    fn persist_pipelined(
        &mut self,
        start_offset: u64,
        records: &[Vec<u8>],
        metrics: &mut EpochPersistMetrics,
    ) -> Result<(), (EpochIoPhase, std::io::Error)> {
        if records.len().saturating_add(1) > self.ring_entries as usize {
            return Err((
                EpochIoPhase::EpochWrite,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "pipelined epoch needs {} SQEs but ring has {} entries",
                        records.len() + 1,
                        self.ring_entries
                    ),
                ),
            ));
        }
        let descriptor = self.descriptor();
        let mut entries = Vec::with_capacity(records.len() + 1);
        let mut expected = Vec::with_capacity(records.len());
        let mut offset = start_offset;
        let mut total = 0usize;
        for (index, record) in records.iter().enumerate() {
            let length = u32::try_from(record.len()).map_err(|_| {
                (
                    EpochIoPhase::EpochWrite,
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("frame {index} exceeds one io_uring write length"),
                    ),
                )
            })?;
            let user_data = write_user_data(index);
            entries.push(
                opcode::Write::new(descriptor, record.as_ptr(), length)
                    .offset(offset)
                    .build()
                    .user_data(user_data),
            );
            expected.push((user_data, record.len()));
            offset += record.len() as u64;
            total += record.len();
        }
        // IO_DRAIN is the completion-ordering barrier. The writes remain
        // independent and may execute concurrently; linking them would
        // serialize the queue and would not test QD > 1.
        entries.push(
            opcode::Fsync::new(descriptor)
                .flags(types::FsyncFlags::DATASYNC)
                .build()
                .flags(squeue::Flags::IO_DRAIN)
                .user_data(FSYNC_USER_DATA),
        );
        metrics.data_writes = records.len() as u64;
        metrics.data_syncs = 1;
        metrics.bytes_written = total as u64;
        metrics.maximum_in_flight_writes = records.len() as u64;
        self.submit_entries(&entries, &expected, entries.len(), metrics)
    }
}

fn invalid_completion(
    phase: EpochIoPhase,
    message: impl Into<String>,
) -> (EpochIoPhase, std::io::Error) {
    (
        phase,
        std::io::Error::new(std::io::ErrorKind::InvalidData, message.into()),
    )
}

fn validate_completion_batch(
    expected_writes: &[(u64, usize)],
    expected_cqes: usize,
    completions: &[(u64, i32)],
) -> Result<(), (EpochIoPhase, std::io::Error)> {
    if completions.len() != expected_cqes {
        return Err(invalid_completion(
            EpochIoPhase::EpochWrite,
            format!(
                "expected {expected_cqes} CQEs, observed {}",
                completions.len()
            ),
        ));
    }

    let mut write_results = vec![None; expected_writes.len()];
    let mut fsync_result = None;
    for (user_data, result) in completions.iter().copied() {
        if user_data == FSYNC_USER_DATA {
            if fsync_result.replace(result).is_some() {
                return Err(invalid_completion(
                    EpochIoPhase::EpochSync,
                    "duplicate fsync completion",
                ));
            }
            continue;
        }
        if user_data & WRITE_USER_DATA_MASK != WRITE_USER_DATA_TAG {
            return Err(invalid_completion(
                EpochIoPhase::EpochWrite,
                format!("unknown completion user_data {user_data:#x}"),
            ));
        }
        let index = (user_data & 0xffff_ffff) as usize;
        let slot = write_results.get_mut(index).ok_or_else(|| {
            invalid_completion(
                EpochIoPhase::EpochWrite,
                format!("out-of-range write completion {index}"),
            )
        })?;
        if slot.replace(result).is_some() {
            return Err(invalid_completion(
                EpochIoPhase::EpochWrite,
                format!("duplicate write completion {index}"),
            ));
        }
    }

    for (index, ((expected_user_data, expected_length), result)) in expected_writes
        .iter()
        .zip(write_results.into_iter())
        .enumerate()
    {
        if *expected_user_data != write_user_data(index) {
            return Err(invalid_completion(
                EpochIoPhase::EpochWrite,
                format!("noncanonical expected write identifier at index {index}"),
            ));
        }
        let result = result.ok_or_else(|| {
            invalid_completion(
                EpochIoPhase::EpochWrite,
                format!("missing write completion {index}"),
            )
        })?;
        if result < 0 {
            return Err((
                EpochIoPhase::EpochWrite,
                std::io::Error::from_raw_os_error(-result),
            ));
        }
        if result as usize != *expected_length {
            return Err((
                EpochIoPhase::EpochWrite,
                std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    format!("io_uring write {index} completed {result} of {expected_length} bytes"),
                ),
            ));
        }
    }

    let fsync_result = fsync_result.ok_or_else(|| {
        invalid_completion(
            EpochIoPhase::EpochSync,
            "missing data-synchronization completion",
        )
    })?;
    if fsync_result < 0 {
        return Err((
            EpochIoPhase::EpochSync,
            std::io::Error::from_raw_os_error(-fsync_result),
        ));
    }
    if fsync_result != 0 {
        return Err(invalid_completion(
            EpochIoPhase::EpochSync,
            format!("io_uring fsync returned unexpected result {fsync_result}"),
        ));
    }
    Ok(())
}

fn write_user_data(index: usize) -> u64 {
    WRITE_USER_DATA_TAG | index as u64
}

#[cfg(target_os = "linux")]
impl EpochFileIo for IoUringEpochFileIo {
    fn persist_epoch(
        &mut self,
        start_offset: u64,
        records: &[Vec<u8>],
    ) -> Option<(
        EpochPersistMetrics,
        Result<(), (EpochIoPhase, std::io::Error)>,
    )> {
        let mut metrics = EpochPersistMetrics::default();
        let result = match self.strategy {
            IoUringEpochStrategy::ContiguousWrite => {
                self.persist_contiguous(start_offset, records, &mut metrics)
            }
            IoUringEpochStrategy::VectoredWrite => {
                self.persist_vectored(start_offset, records, &mut metrics)
            }
            IoUringEpochStrategy::PipelinedWrites => {
                self.persist_pipelined(start_offset, records, &mut metrics)
            }
        };
        Some((metrics, result))
    }

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

#[cfg(not(target_os = "linux"))]
impl EpochFileIo for IoUringEpochFileIo {
    fn persist_epoch(
        &mut self,
        _start_offset: u64,
        _records: &[Vec<u8>],
    ) -> Option<(
        EpochPersistMetrics,
        Result<(), (EpochIoPhase, std::io::Error)>,
    )> {
        Some((
            EpochPersistMetrics::default(),
            Err((
                EpochIoPhase::EpochWrite,
                std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "io_uring epoch transport is supported only on Linux",
                ),
            )),
        ))
    }

    fn len(&mut self, _phase: EpochIoPhase) -> std::io::Result<u64> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "io_uring epoch transport is supported only on Linux",
        ))
    }

    fn write_at(
        &mut self,
        _phase: EpochIoPhase,
        _offset: u64,
        _bytes: &[u8],
    ) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "io_uring epoch transport is supported only on Linux",
        ))
    }

    fn sync_data(&mut self, _phase: EpochIoPhase) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "io_uring epoch transport is supported only on Linux",
        ))
    }

    fn set_len(&mut self, _phase: EpochIoPhase, _len: u64) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "io_uring epoch transport is supported only on Linux",
        ))
    }

    fn read_all(&mut self, _phase: EpochIoPhase) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "io_uring epoch transport is supported only on Linux",
        ))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "forthdb-io-uring-epoch-{name}-{}-{sequence}.db",
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

    fn unavailable(error: &FileEpochStoreError) -> bool {
        match error {
            FileEpochStoreError::Io { source, .. } => {
                matches!(source.raw_os_error(), Some(1 | 38 | 95))
            }
            _ => false,
        }
    }

    fn open_or_skip(path: &Path, strategy: IoUringEpochStrategy) -> Option<IoUringEpochStore> {
        match IoUringEpochFileIo::open_store_with_entries(path, strategy, 64) {
            Ok(store) => Some(store),
            Err(error) if unavailable(&error) => {
                eprintln!("io_uring unavailable on this kernel or runner: {error}");
                None
            }
            Err(error) => panic!("io_uring epoch store should open: {error}"),
        }
    }

    fn fact(index: usize) -> Fact {
        Fact::new(
            Atom::Literal(Literal::new(format!("subject-{index}"))),
            Predicate::new("state"),
            Atom::Literal(Literal::new(format!("value-{index}"))),
        )
    }

    fn frames(count: usize) -> Vec<Arc<CommitFrame>> {
        let database = Database::new(MemoryCommitStore::new()).expect("memory database opens");
        for index in 0..count {
            let mut transaction = database.begin();
            transaction.define(SlotId::new(format!("slot/{index}")), fact(index));
            database
                .commit(transaction)
                .expect("memory commit succeeds");
        }
        database.frames()
    }

    #[test]
    fn completion_validation_rejects_missing_duplicate_unknown_short_and_failed_cqes() {
        let expected = vec![(write_user_data(0), 11), (write_user_data(1), 17)];
        let valid = vec![
            (write_user_data(1), 17),
            (FSYNC_USER_DATA, 0),
            (write_user_data(0), 11),
        ];
        validate_completion_batch(&expected, 3, &valid)
            .expect("out-of-order valid completions are accepted");

        let missing = vec![(write_user_data(0), 11), (FSYNC_USER_DATA, 0)];
        assert!(matches!(
            validate_completion_batch(&expected, 3, &missing),
            Err((EpochIoPhase::EpochWrite, _))
        ));

        let duplicate = vec![
            (write_user_data(0), 11),
            (write_user_data(0), 11),
            (FSYNC_USER_DATA, 0),
        ];
        assert!(matches!(
            validate_completion_batch(&expected, 3, &duplicate),
            Err((EpochIoPhase::EpochWrite, _))
        ));

        let unknown = vec![
            (write_user_data(0), 11),
            (0xdead_beef, 17),
            (FSYNC_USER_DATA, 0),
        ];
        assert!(matches!(
            validate_completion_batch(&expected, 3, &unknown),
            Err((EpochIoPhase::EpochWrite, _))
        ));

        let short = vec![
            (write_user_data(0), 10),
            (write_user_data(1), 17),
            (FSYNC_USER_DATA, 0),
        ];
        let (_, short_error) =
            validate_completion_batch(&expected, 3, &short).expect_err("short write fails");
        assert_eq!(short_error.kind(), std::io::ErrorKind::WriteZero);

        let failed_write = vec![
            (write_user_data(0), -5),
            (write_user_data(1), 17),
            (FSYNC_USER_DATA, 0),
        ];
        let (phase, failed_write_error) = validate_completion_batch(&expected, 3, &failed_write)
            .expect_err("negative write result fails");
        assert_eq!(phase, EpochIoPhase::EpochWrite);
        assert_eq!(failed_write_error.raw_os_error(), Some(5));

        let failed_sync = vec![
            (write_user_data(0), 11),
            (write_user_data(1), 17),
            (FSYNC_USER_DATA, -5),
        ];
        let (phase, failed_sync_error) = validate_completion_batch(&expected, 3, &failed_sync)
            .expect_err("negative sync result fails");
        assert_eq!(phase, EpochIoPhase::EpochSync);
        assert_eq!(failed_sync_error.raw_os_error(), Some(5));
    }

    #[test]
    fn all_ring_epoch_strategies_match_established_v1_bytes() {
        let frames = frames(3);
        let control = TempFile::new("control");
        let control_database =
            Database::new(FileCommitStore::open(control.path()).expect("file control opens"))
                .expect("control reconstructs");
        for frame in &frames {
            control_database
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .append(frame.clone())
                .expect("control frame appends");
        }
        drop(control_database);
        let expected = fs::read(control.path()).expect("control bytes read");

        for strategy in [
            IoUringEpochStrategy::ContiguousWrite,
            IoUringEpochStrategy::VectoredWrite,
            IoUringEpochStrategy::PipelinedWrites,
        ] {
            let temp = TempFile::new(strategy.as_str());
            let Some(mut store) = open_or_skip(temp.path(), strategy) else {
                return;
            };
            store.append_epoch(&frames).expect("ring epoch commits");
            assert_eq!(fs::read(temp.path()).expect("ring bytes read"), expected);
            assert_eq!(store.len(), frames.len());
            let metrics = store.metrics();
            assert_eq!(metrics.data_syncs, 1);
            assert_eq!(metrics.submission_calls, 1);
            match strategy {
                IoUringEpochStrategy::ContiguousWrite => {
                    assert_eq!(metrics.data_writes, 1);
                    assert_eq!(metrics.completion_events, 2);
                    assert_eq!(metrics.maximum_in_flight_writes, 1);
                    assert!(metrics.arena_bytes_copied > 0);
                    assert_eq!(metrics.iovecs_submitted, 0);
                }
                IoUringEpochStrategy::VectoredWrite => {
                    assert_eq!(metrics.data_writes, 1);
                    assert_eq!(metrics.completion_events, 2);
                    assert_eq!(metrics.maximum_in_flight_writes, 1);
                    assert_eq!(metrics.arena_bytes_copied, 0);
                    assert_eq!(metrics.iovecs_submitted, frames.len() as u64);
                }
                IoUringEpochStrategy::PipelinedWrites => {
                    assert_eq!(metrics.data_writes, frames.len() as u64);
                    assert_eq!(metrics.completion_events, frames.len() as u64 + 1);
                    assert_eq!(metrics.maximum_in_flight_writes, frames.len() as u64);
                    assert_eq!(metrics.arena_bytes_copied, 0);
                }
            }
            drop(store);
            let reopened = FileCommitStore::open(temp.path()).expect("file recovery reopens");
            assert_eq!(reopened.len(), frames.len());
        }
    }

    #[test]
    fn durable_controller_waits_for_ring_epoch_before_resolving_tickets() {
        let temp = TempFile::new("controller");
        let Some(store) = open_or_skip(temp.path(), IoUringEpochStrategy::PipelinedWrites) else {
            return;
        };
        let database = Arc::new(Database::new(store).expect("ring database reconstructs"));
        let controller = DurableQueuedIntentController::new(database.clone(), 32, 8)
            .expect("durable controller starts");
        let mut tickets = Vec::new();
        for index in 0..8 {
            let mut intent = QueuedIntent::new();
            intent.define_fact(SlotId::new(format!("queued/{index}")), fact(index));
            tickets.push(controller.submit(intent).expect("intent admitted"));
        }
        for ticket in tickets {
            match ticket.wait().expect("ticket resolves") {
                DurableTicketOutcome::Accepted { .. } => {}
                other => panic!("expected durable acceptance, found {other:?}"),
            }
        }
        controller.flush().expect("controller drains");
        assert_eq!(database.snapshot().version(), 8);
        assert_eq!(database.frame_count(), 8);
        let metrics = controller.store_metrics();
        assert_eq!(metrics.data_syncs, 1);
        assert_eq!(metrics.maximum_in_flight_writes, 8);
        drop(controller);
        drop(database);
        let reopened = FileCommitStore::open(temp.path()).expect("ring history reopens");
        assert_eq!(reopened.len(), 8);
    }
}
