use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum WriterLeaseError {
    AlreadyHeld(PathBuf),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Unsupported,
}

impl fmt::Display for WriterLeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyHeld(path) => {
                write!(
                    formatter,
                    "another process holds the ForthDB writer lease at {}",
                    path.display()
                )
            }
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "writer lease I/O failed at {}: {source}",
                    path.display()
                )
            }
            Self::Unsupported => write!(
                formatter,
                "process-scoped writer leases are unsupported on this platform"
            ),
        }
    }
}

impl Error for WriterLeaseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::AlreadyHeld(_) | Self::Unsupported => None,
        }
    }
}

#[derive(Debug)]
pub struct WriterLease {
    path: PathBuf,
    file: File,
}

impl WriterLease {
    pub fn acquire(database_path: impl AsRef<Path>) -> Result<Self, WriterLeaseError> {
        let path = lock_path(database_path.as_ref());
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| WriterLeaseError::Io {
                path: path.clone(),
                source,
            })?;

        acquire_platform_lock(&file, &path)?;

        // Diagnostic metadata only. Ownership is the kernel lock, not these bytes.
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = writeln!(file, "pid={}", std::process::id());
        let _ = file.flush();

        Ok(Self { path, file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        release_platform_lock(&self.file);
    }
}

pub fn lock_path(database_path: &Path) -> PathBuf {
    let mut value = database_path.as_os_str().to_os_string();
    value.push(".writer.lock");
    PathBuf::from(value)
}

#[cfg(unix)]
fn acquire_platform_lock(file: &File, path: &Path) -> Result<(), WriterLeaseError> {
    use std::os::fd::AsRawFd;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(());
    }
    let source = std::io::Error::last_os_error();
    if matches!(source.raw_os_error(), Some(libc::EWOULDBLOCK)) {
        Err(WriterLeaseError::AlreadyHeld(path.to_path_buf()))
    } else {
        Err(WriterLeaseError::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[cfg(unix)]
fn release_platform_lock(file: &File) {
    use std::os::fd::AsRawFd;

    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

#[cfg(not(unix))]
fn acquire_platform_lock(_file: &File, _path: &Path) -> Result<(), WriterLeaseError> {
    Err(WriterLeaseError::Unsupported)
}

#[cfg(not(unix))]
fn release_platform_lock(_file: &File) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "forthdb-writer-lease-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn separate_open_file_descriptions_conflict_and_release() {
        let database = temp_path();
        let first = WriterLease::acquire(&database).expect("first lease acquired");
        assert!(matches!(
            WriterLease::acquire(&database),
            Err(WriterLeaseError::AlreadyHeld(_))
        ));
        drop(first);
        let second = WriterLease::acquire(&database).expect("lease released");
        drop(second);
        let _ = std::fs::remove_file(lock_path(&database));
    }
}
