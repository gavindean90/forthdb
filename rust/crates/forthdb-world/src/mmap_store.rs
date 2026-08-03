use super::file_store::{FileCommitStore, FileCommitStoreError};
use super::*;
use memmap2::{Mmap, MmapOptions};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
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

pub type MmapCommitStoreError = FileCommitStoreError;

pub struct MmapCommitStore {
    path: PathBuf,
    file: File,
    mapping: Option<Mmap>,
    frames: Vec<Arc<CommitFrame>>,
    recovered_tail_bytes: u64,
    writer: Option<FileCommitStore>,
    last_remap_error: Option<String>,
}

impl fmt::Debug for MmapCommitStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MmapCommitStore")
            .field("path", &self.path)
            .field("mapped_len", &self.mapped_len())
            .field("frames", &self.frames.len())
            .field("recovered_tail_bytes", &self.recovered_tail_bytes)
            .field("mapping_is_current", &self.mapping_is_current())
            .field("last_remap_error", &self.last_remap_error)
            .finish()
    }
}

impl MmapCommitStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MmapCommitStoreError> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)?;

        if file.metadata()?.len() == 0 {
            write_file_header(&mut file)?;
            file.sync_data()?;
        }

        let mut mapping = map_file(&file)?;
        let (frames, last_good_offset) = decode_file(&mapping)?;
        let recovered_tail_bytes = mapping.len().saturating_sub(last_good_offset) as u64;

        if recovered_tail_bytes != 0 {
            drop(mapping);
            file.set_len(last_good_offset as u64)?;
            file.sync_data()?;
            mapping = map_file(&file)?;
        }

        World::reconstruct(&frames).map_err(FileCommitStoreError::History)?;
        file.seek(SeekFrom::End(0))?;

        Ok(Self {
            path,
            file,
            mapping: Some(mapping),
            frames,
            recovered_tail_bytes,
            writer: None,
            last_remap_error: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn recovered_tail_bytes(&self) -> u64 {
        self.recovered_tail_bytes
    }

    pub fn file_len(&self) -> Result<u64, std::io::Error> {
        self.file.metadata().map(|metadata| metadata.len())
    }

    pub fn mapped_len(&self) -> usize {
        self.mapping.as_ref().map_or(0, |mapping| mapping.len())
    }

    pub fn mapped_bytes(&self) -> Option<&[u8]> {
        self.mapping.as_deref()
    }

    pub fn mapping_is_current(&self) -> bool {
        match (self.mapping.as_ref(), self.file.metadata()) {
            (Some(mapping), Ok(metadata)) => mapping.len() as u64 == metadata.len(),
            _ => false,
        }
    }

    pub fn last_remap_error(&self) -> Option<&str> {
        self.last_remap_error.as_deref()
    }

    pub fn refresh_mapping(&mut self) -> Result<(), MmapCommitStoreError> {
        self.mapping.take();
        let mapping = map_file(&self.file)?;
        self.mapping = Some(mapping);
        self.last_remap_error = None;
        Ok(())
    }

    fn validate_append(&self, frame: &CommitFrame) -> Result<(), MmapCommitStoreError> {
        let (expected_parent, expected_parent_version) = self
            .frames
            .last()
            .map(|current| (current.resulting_world(), current.resulting_version()))
            .unwrap_or((WorldId::GENESIS, 0));

        if frame.parent_world() != expected_parent {
            return Err(FileCommitStoreError::NonLinearAppend(format!(
                "expected parent {expected_parent}, found {}",
                frame.parent_world()
            )));
        }
        if frame.parent_version() != expected_parent_version {
            return Err(FileCommitStoreError::NonLinearAppend(format!(
                "expected parent version {expected_parent_version}, found {}",
                frame.parent_version()
            )));
        }
        let expected_resulting_version = expected_parent_version.checked_add(1).ok_or_else(|| {
            FileCommitStoreError::NonLinearAppend("world version overflow".to_owned())
        })?;
        if frame.resulting_version() != expected_resulting_version {
            return Err(FileCommitStoreError::NonLinearAppend(format!(
                "expected resulting version {expected_resulting_version}, found {}",
                frame.resulting_version()
            )));
        }
        Ok(())
    }

    fn remap_after_durable_append(&mut self) {
        self.mapping.take();
        match map_file(&self.file) {
            Ok(mapping) => {
                self.mapping = Some(mapping);
                self.last_remap_error = None;
            }
            Err(error) => {
                self.last_remap_error = Some(error.to_string());
            }
        }
    }
}

impl CommitStore for MmapCommitStore {
    type Error = MmapCommitStoreError;

    fn append(&mut self, frame: Arc<CommitFrame>) -> Result<(), Self::Error> {
        self.validate_append(&frame)?;

        if self.writer.is_none() {
            self.writer = Some(FileCommitStore::open(&self.path)?);
        }
        self.writer
            .as_mut()
            .expect("writer was initialized")
            .append(frame.clone())?;

        self.frames.push(frame);
        self.recovered_tail_bytes = 0;
        self.remap_after_durable_append();
        Ok(())
    }

    fn frames(&self) -> Vec<Arc<CommitFrame>> {
        self.frames.clone()
    }

    fn len(&self) -> usize {
        self.frames.len()
    }
}

fn map_file(file: &File) -> Result<Mmap, FileCommitStoreError> {
    // SAFETY: the store keeps the file alive for at least as long as the mapping,
    // never truncates while a mapping is live, and only grows the file through its
    // own append path. Cross-process mutation remains outside this milestone.
    unsafe { MmapOptions::new().map(file) }.map_err(FileCommitStoreError::Io)
}

fn write_file_header(file: &mut File) -> Result<(), std::io::Error> {
    file.seek(SeekFrom::Start(0))?;
    file.write_all(FILE_MAGIC)?;
    file.write_all(&FILE_VERSION.to_le_bytes())?;
    file.write_all(&FILE_FLAGS.to_le_bytes())?;
    Ok(())
}

include!("mmap_format.rs");

#[cfg(test)]
mod mmap_tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "forthdb-mmap-{name}-{}-{sequence}.log",
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

    fn write_one_world(path: &Path) -> (WorldId, SlotId) {
        let store = FileCommitStore::open(path).expect("file store opens");
        let database = Database::new(store).expect("empty file history is valid");
        let mut transaction = database.begin();
        let entity = transaction.entity();
        let slot = SlotId::new("mapped/state");
        transaction.define(slot.clone(), state_fact(entity, "ready"));
        let world = database.commit(transaction).expect("file commit succeeds");
        (world.id(), slot)
    }

    #[test]
    fn mapped_open_reconstructs_file_store_history() {
        let temp = TempFile::new("reopen");
        let (world_id, slot) = write_one_world(temp.path());

        let store = MmapCommitStore::open(temp.path()).expect("mmap store opens");
        assert_eq!(store.len(), 1);
        assert!(store.mapping_is_current());
        assert_eq!(&store.mapped_bytes().expect("mapping")[..8], FILE_MAGIC);
        let database = Database::new(store).expect("mapped history reconstructs");
        assert_eq!(database.snapshot().id(), world_id);
        assert!(database.snapshot().resolve(&slot).is_some());
    }

    #[test]
    fn mapped_store_appends_using_the_canonical_file_writer() {
        let temp = TempFile::new("append");
        let store = MmapCommitStore::open(temp.path()).expect("mmap store opens");
        let database = Database::new(store).expect("empty mapped history is valid");
        let mut transaction = database.begin();
        let entity = transaction.entity();
        let slot = SlotId::new("mapped/append");
        transaction.define(slot.clone(), state_fact(entity, "committed"));
        let world = database.commit(transaction).expect("mapped commit succeeds");
        drop(database);

        let file_store = FileCommitStore::open(temp.path()).expect("file store reopens mmap bytes");
        let reopened = Database::new(file_store).expect("canonical bytes reconstruct");
        assert_eq!(reopened.snapshot().id(), world.id());
        assert!(reopened.snapshot().resolve(&slot).is_some());
    }

    #[test]
    fn mapped_open_recovers_and_remaps_an_incomplete_tail() {
        let temp = TempFile::new("tail");
        write_one_world(temp.path());
        let clean_len = fs::metadata(temp.path()).expect("metadata").len();

        let mut file = OpenOptions::new()
            .append(true)
            .open(temp.path())
            .expect("append tail");
        file.write_all(b"FRM1\x10\x00\x00")
            .expect("write incomplete tail");
        file.sync_data().expect("sync incomplete tail");
        drop(file);

        let store = MmapCommitStore::open(temp.path()).expect("tail recovers");
        assert_eq!(store.recovered_tail_bytes(), 7);
        assert_eq!(store.mapped_len() as u64, clean_len);
        assert!(store.mapping_is_current());
    }

    #[test]
    fn mapped_checksum_corruption_fails_closed() {
        let temp = TempFile::new("checksum");
        write_one_world(temp.path());
        let mut bytes = fs::read(temp.path()).expect("read history");
        bytes[FILE_HEADER_LEN + FRAME_PREFIX_LEN] ^= 0x01;
        fs::write(temp.path(), bytes).expect("write corruption");

        let error = MmapCommitStore::open(temp.path()).expect_err("corruption must fail");
        assert!(matches!(error, FileCommitStoreError::CorruptFrame { .. }));
    }

    #[test]
    fn file_and_mmap_stores_decode_identical_frames() {
        let temp = TempFile::new("parity");
        write_one_world(temp.path());
        let file_frames = FileCommitStore::open(temp.path())
            .expect("file store opens")
            .frames();
        let mmap_frames = MmapCommitStore::open(temp.path())
            .expect("mmap store opens")
            .frames();
        assert_eq!(file_frames, mmap_frames);
    }
}
