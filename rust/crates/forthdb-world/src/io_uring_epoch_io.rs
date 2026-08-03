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

#[cfg(target_os = "linux")]
pub struct IoUringEpochFileIo {
    path: PathBuf,
    file: File,
    ring: Option<IoUring>,
    ring_entries: u32,
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub struct IoUringEpochFileIo {
    path: PathBuf,
}

#[cfg(target_os = "linux")]
impl std::fmt::Debug for IoUringEpochFileIo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IoUringEpochFileIo")
            .field("path", &self.path)
            .field("ring_entries", &self.ring_entries)
            .field("ring_alive", &self.ring.is_some())
            .finish()
    }
}

pub type IoUringEpochStore = FileEpochStore<IoUringEpochFileIo>;

#[cfg(target_os = "linux")]
pub(crate) struct PendingIoUringEpoch {
    // The SQEs retain this allocation's pointer until both CQEs are reaped.
    _arena: Vec<u8>,
    expected_writes: Vec<(u64, usize)>,
    expected_cqes: usize,
    metrics: EpochPersistMetrics,
}

impl IoUringEpochFileIo {
    pub fn open_store(path: impl AsRef<Path>) -> Result<IoUringEpochStore, FileEpochStoreError> {
        Self::open_store_with_entries(path, DEFAULT_RING_ENTRIES)
    }

    #[cfg(target_os = "linux")]
    pub fn open_store_with_entries(
        path: impl AsRef<Path>,
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
        let io = Self::open(&path, ring_entries).map_err(|source| FileEpochStoreError::Io {
            phase: EpochIoPhase::VerifyRead,
            source,
        })?;
        FileEpochStore::from_io(path, io, FileEpochSyncPolicy::PerEpoch)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn open_store_with_entries(
        path: impl AsRef<Path>,
        _ring_entries: u32,
    ) -> Result<IoUringEpochStore, FileEpochStoreError> {
        let _ = path.as_ref();
        Err(FileEpochStoreError::Io {
            phase: EpochIoPhase::VerifyRead,
            source: std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "io_uring epoch transport is supported only on Linux",
            ),
        })
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
    fn open(path: impl AsRef<Path>, ring_entries: u32) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        let ring = IoUring::new(ring_entries)?;
        Ok(Self {
            path,
            file,
            ring: Some(ring),
            ring_entries,
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

    /// Submit one contiguous epoch without waiting for durability.
    ///
    /// The returned value owns every buffer referenced by the kernel. Callers
    /// must pass it to `complete_contiguous_epoch` before it is dropped.
    pub(crate) fn submit_contiguous_epoch(
        &mut self,
        start_offset: u64,
        records: &[Vec<u8>],
    ) -> Result<PendingIoUringEpoch, (EpochIoPhase, std::io::Error)> {
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

        let descriptor = self.descriptor();
        let entries = [
            opcode::Write::new(descriptor, arena.as_ptr(), length)
                .offset(start_offset)
                .build()
                .flags(squeue::Flags::IO_LINK)
                .user_data(write_user_data(0)),
            opcode::Fsync::new(descriptor)
                .flags(types::FsyncFlags::DATASYNC)
                .build()
                .user_data(FSYNC_USER_DATA),
        ];
        let submit_result = (|| {
            let ring = self.require_ring()?;
            {
                let mut submission = ring.submission();
                // SAFETY: `arena` is moved into the returned pending epoch and
                // retained until completion reaps both CQEs.
                unsafe {
                    submission.push_multiple(&entries).map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            "io_uring submission queue could not accept the epoch",
                        )
                    })?;
                }
            }
            // A short submission is not an error: SQEs left in the userspace
            // queue are submitted by `submit_and_wait` during completion.
            ring.submit().map(|_| ())
        })();
        if let Err(error) = submit_result {
            let reset = self.discard_and_recreate_ring().err();
            return Err((EpochIoPhase::EpochWrite, reset.unwrap_or(error)));
        }

        Ok(PendingIoUringEpoch {
            _arena: arena,
            expected_writes: vec![(write_user_data(0), total)],
            expected_cqes: entries.len(),
            metrics: EpochPersistMetrics {
                data_writes: 1,
                data_syncs: 1,
                bytes_written: total as u64,
                submission_calls: 1,
                maximum_in_flight_writes: 1,
                arena_bytes_copied: total as u64,
                ..EpochPersistMetrics::default()
            },
        })
    }

    pub(crate) fn complete_contiguous_epoch(
        &mut self,
        mut pending: PendingIoUringEpoch,
    ) -> (
        EpochPersistMetrics,
        Result<(), (EpochIoPhase, std::io::Error)>,
    ) {
        let result = (|| {
            let ring = self
                .require_ring()
                .map_err(|error| (EpochIoPhase::EpochWrite, error))?;
            ring.submit_and_wait(pending.expected_cqes)
                .map_err(|error| (EpochIoPhase::EpochWrite, error))?;

            let mut completions = Vec::with_capacity(pending.expected_cqes);
            {
                let ring = self
                    .require_ring()
                    .map_err(|error| (EpochIoPhase::EpochWrite, error))?;
                let mut completion = ring.completion();
                while completions.len() < pending.expected_cqes {
                    let Some(entry) = completion.next() else {
                        break;
                    };
                    completions.push((entry.user_data(), entry.result()));
                }
            }
            pending.metrics.completion_events += completions.len() as u64;
            validate_completion_batch(
                &pending.expected_writes,
                pending.expected_cqes,
                &completions,
            )
        })();

        if result.is_err() {
            if let Err(reset) = self.discard_and_recreate_ring() {
                return (pending.metrics, Err((EpochIoPhase::EpochWrite, reset)));
            }
        }
        (pending.metrics, result)
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
}
