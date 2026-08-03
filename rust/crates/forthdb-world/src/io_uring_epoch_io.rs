use super::*;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use io_uring::{opcode, squeue, types, IoUring};
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
                        entries.len(), self.ring_entries
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

        if let Err(error) = validate_completion_batch(expected_writes, expected_cqes, &completions) {
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
        &