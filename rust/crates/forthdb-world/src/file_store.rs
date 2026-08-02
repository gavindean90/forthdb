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

#[derive(Debug)]
pub enum FileCommitStoreError {
    Io(std::io::Error),
    InvalidHeader(String),
    UnsupportedFormat(u32),
    CorruptFrame { offset: u64, reason: String },
    NonLinearAppend(String),
    History(CandidateError),
}

impl fmt::Display for FileCommitStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "file commit store I/O failed: {error}"),
            Self::InvalidHeader(message) => write!(formatter, "invalid ForthDB file header: {message}"),
            Self::UnsupportedFormat(version) => {
                write!(formatter, "unsupported ForthDB file format version {version}")
            }
            Self::CorruptFrame { offset, reason } => {
                write!(formatter, "corrupt commit frame at byte {offset}: {reason}")
            }
            Self::NonLinearAppend(message) => write!(formatter, "nonlinear commit append: {message}"),
            Self::History(error) => write!(formatter, "persisted history is invalid: {error}"),
        }
    }
}

impl Error for FileCommitStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::History(error) => Some(error),
            Self::InvalidHeader(_)
            | Self::UnsupportedFormat(_)
            | Self::CorruptFrame { .. }
            | Self::NonLinearAppend(_) => None,
        }
    }
}

impl From<std::io::Error> for FileCommitStoreError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug)]
pub struct FileCommitStore {
    path: PathBuf,
    file: File,
    frames: Vec<Arc<CommitFrame>>,
    recovered_tail_bytes: u64,
}

impl FileCommitStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, FileCommitStoreError> {
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

        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let (frames, last_good_offset) = decode_file(&bytes)?;
        let recovered_tail_bytes = bytes.len().saturating_sub(last_good_offset) as u64;

        if recovered_tail_bytes != 0 {
            file.set_len(last_good_offset as u64)?;
            file.sync_data()?;
        }

        World::reconstruct(&frames).map_err(FileCommitStoreError::History)?;
        file.seek(SeekFrom::End(0))?;

        Ok(Self {
            path,
            file,
            frames,
            recovered_tail_bytes,
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

    fn validate_append(&self, frame: &CommitFrame) -> Result<(), FileCommitStoreError> {
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

    fn rollback_failed_append(&mut self, start: u64) {
        let _ = self.file.set_len(start);
        let _ = self.file.seek(SeekFrom::End(0));
        let _ = self.file.sync_data();
    }
}

impl CommitStore for FileCommitStore {
    type Error = FileCommitStoreError;

    fn append(&mut self, frame: Arc<CommitFrame>) -> Result<(), Self::Error> {
        self.validate_append(&frame)?;
        let record = encode_record(&frame)?;
        let start = self.file.seek(SeekFrom::End(0))?;

        if let Err(error) = self.file.write_all(&record) {
            self.rollback_failed_append(start);
            return Err(FileCommitStoreError::Io(error));
        }
        if let Err(error) = self.file.sync_data() {
            self.rollback_failed_append(start);
            return Err(FileCommitStoreError::Io(error));
        }

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

fn write_file_header(file: &mut File) -> Result<(), std::io::Error> {
    file.seek(SeekFrom::Start(0))?;
    file.write_all(FILE_MAGIC)?;
    file.write_all(&FILE_VERSION.to_le_bytes())?;
    file.write_all(&FILE_FLAGS.to_le_bytes())?;
    Ok(())
}

fn decode_file(bytes: &[u8]) -> Result<(Vec<Arc<CommitFrame>>, usize), FileCommitStoreError> {
    if bytes.len() < FILE_HEADER_LEN {
        return Err(FileCommitStoreError::InvalidHeader(
            "header is incomplete".to_owned(),
        ));
    }
    if &bytes[..8] != FILE_MAGIC {
        return Err(FileCommitStoreError::InvalidHeader(
            "magic bytes do not match FTHDB001".to_owned(),
        ));
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().expect("fixed header slice"));
    if version != FILE_VERSION {
        return Err(FileCommitStoreError::UnsupportedFormat(version));
    }
    let flags = u32::from_le_bytes(bytes[12..16].try_into().expect("fixed header slice"));
    if flags != FILE_FLAGS {
        return Err(FileCommitStoreError::InvalidHeader(format!(
            "unsupported header flags 0x{flags:08x}"
        )));
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
            return Err(corrupt(offset, "frame magic does not match FRM1"));
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
            ));
        }
        let payload_len = usize::try_from(payload_len)
            .map_err(|_| corrupt(offset, "payload length does not fit this platform"))?;
        let total_len = FRAME_PREFIX_LEN
            .checked_add(payload_len)
            .and_then(|value| value.checked_add(FRAME_TRAILER_LEN))
            .ok_or_else(|| corrupt(offset, "record length overflow"))?;
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
            return Err(corrupt(offset, "completion trailer does not match END1"));
        }

        let payload = &bytes[payload_start..payload_end];
        let actual_checksum = checksum(payload);
        if actual_checksum != expected_checksum {
            return Err(corrupt(
                offset,
                format!(
                    "checksum mismatch: expected {expected_checksum:016x}, calculated {actual_checksum:016x}"
                ),
            ));
        }

        let frame = Arc::new(
            decode_payload(payload)
                .map_err(|reason| corrupt(offset, format!("invalid payload: {reason}")))?,
        );
        validate_decoded_position(&frames, &frame, offset)?;
        frames.push(frame);
        offset += total_len;
        last_good_offset = offset;
    }

    Ok((frames, last_good_offset))
}

fn validate_decoded_position(
    frames: &[Arc<CommitFrame>],
    frame: &CommitFrame,
    offset: usize,
) -> Result<(), FileCommitStoreError> {
    let (expected_parent, expected_parent_version) = frames
        .last()
        .map(|current| (current.resulting_world(), current.resulting_version()))
        .unwrap_or((WorldId::GENESIS, 0));

    if frame.parent_world() != expected_parent {
        return Err(corrupt(
            offset,
            format!(
                "expected parent {expected_parent}, found {}",
                frame.parent_world()
            ),
        ));
    }
    if frame.parent_version() != expected_parent_version {
        return Err(corrupt(
            offset,
            format!(
                "expected parent version {expected_parent_version}, found {}",
                frame.parent_version()
            ),
        ));
    }
    let expected_resulting_version = expected_parent_version
        .checked_add(1)
        .ok_or_else(|| corrupt(offset, "world version overflow"))?;
    if frame.resulting_version() != expected_resulting_version {
        return Err(corrupt(
            offset,
            format!(
                "expected resulting version {expected_resulting_version}, found {}",
                frame.resulting_version()
            ),
        ));
    }
    Ok(())
}

fn corrupt(offset: usize, reason: impl Into<String>) -> FileCommitStoreError {
    FileCommitStoreError::CorruptFrame {
        offset: offset as u64,
        reason: reason.into(),
    }
}

fn encode_record(frame: &CommitFrame) -> Result<Vec<u8>, FileCommitStoreError> {
    let payload = encode_payload(frame);
    if payload.len() as u64 > MAX_FRAME_BYTES {
        return Err(FileCommitStoreError::NonLinearAppend(format!(
            "encoded frame payload is {} bytes, limit is {MAX_FRAME_BYTES}",
            payload.len()
        )));
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

fn decode_payload(payload: &[u8]) -> Result<CommitFrame, String> {
    let mut decoder = Decoder::new(payload);
    let parent_world = WorldId::new(decoder.u64()?);
    let resulting_world = WorldId::new(decoder.u64()?);
    let parent_version = decoder.u64()?;
    let resulting_version = decoder.u64()?;
    let resulting_allocator = decoder.u64()?;
    let operation_count = decoder.u64()?;
    if operation_count > MAX_OPERATIONS_PER_FRAME {
        return Err(format!(
            "operation count {operation_count} exceeds limit {MAX_OPERATIONS_PER_FRAME}"
        ));
    }

    let mut operations = Vec::with_capacity(operation_count as usize);
    for _ in 0..operation_count {
        let operation = match decoder.byte()? {
            0 => Operation::AllocateEntity {
                entity: EntityId::new(decoder.u64()?),
            },
            1 => Operation::Define {
                slot: SlotId::new(decoder.string()?),
                fact: Fact::new(decoder.atom()?, Predicate::new(decoder.string()?), decoder.atom()?),
            },
            2 => Operation::Forget {
                slot: SlotId::new(decoder.string()?),
            },
            tag => return Err(format!("unknown operation tag {tag}")),
        };
        operations.push(operation);
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

fn checksum(bytes: &[u8]) -> u64 {
    let mut value = CHECKSUM_OFFSET_BASIS;
    for byte in bytes {
        value ^= u64::from(*byte);
        value = value.wrapping_mul(CHECKSUM_PRIME);
    }
    value
}

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| "payload cursor overflow".to_owned())?;
        if end > self.bytes.len() {
            return Err(format!(
                "unexpected end of payload at byte {}, need {length} more bytes",
                self.position
            ));
        }
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }

    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(
            self.take(8)?
                .try_into()
                .expect("decoder requested exactly eight bytes"),
        ))
    }

    fn string(&mut self) -> Result<String, String> {
        let length = self.u64()?;
        let length = usize::try_from(length)
            .map_err(|_| "string length does not fit this platform".to_owned())?;
        let bytes = self.take(length)?;
        let value = std::str::from_utf8(bytes)
            .map_err(|error| format!("string is not valid UTF-8: {error}"))?;
        Ok(value.to_owned())
    }

    fn atom(&mut self) -> Result<Atom, String> {
        match self.byte()? {
            0 => Ok(Atom::Entity(EntityId::new(self.u64()?))),
            1 => Ok(Atom::Literal(Literal::new(self.string()?))),
            tag => Err(format!("unknown atom tag {tag}")),
        }
    }

    fn finish(self) -> Result<(), String> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(format!(
                "{} trailing payload bytes remain",
                self.bytes.len() - self.position
            ))
        }
    }
}

#[cfg(test)]
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
                "forthdb-{name}-{}-{sequence}.log",
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
        let slot = SlotId::new("durable/state");
        transaction.define(slot.clone(), state_fact(entity, "ready"));
        let world = database.commit(transaction).expect("file commit succeeds");
        (world.id(), slot)
    }

    #[test]
    fn durable_commit_reopens_to_the_same_world() {
        let temp = TempFile::new("reopen");
        let (world_id, slot) = write_one_world(temp.path());

        let store = FileCommitStore::open(temp.path()).expect("file store reopens");
        assert_eq!(store.len(), 1);
        assert_eq!(store.recovered_tail_bytes(), 0);
        let database = Database::new(store).expect("persisted history reconstructs");
        assert_eq!(database.snapshot().id(), world_id);
        assert_eq!(database.snapshot().version(), 1);
        assert!(database.snapshot().resolve(&slot).is_some());
    }

    #[test]
    fn incomplete_final_record_is_ignored_and_truncated() {
        let temp = TempFile::new("tail");
        write_one_world(temp.path());
        let clean_len = fs::metadata(temp.path()).expect("metadata exists").len();

        let mut file = OpenOptions::new()
            .append(true)
            .open(temp.path())
            .expect("append tail");
        file.write_all(b"FRM1\x10\x00\x00")
            .expect("write incomplete record");
        file.sync_data().expect("sync test tail");
        drop(file);

        let store = FileCommitStore::open(temp.path()).expect("incomplete tail is recoverable");
        assert_eq!(store.len(), 1);
        assert_eq!(store.recovered_tail_bytes(), 7);
        assert_eq!(store.file_len().expect("file length"), clean_len);
    }

    #[test]
    fn checksum_corruption_fails_closed() {
        let temp = TempFile::new("checksum");
        write_one_world(temp.path());
        let mut bytes = fs::read(temp.path()).expect("read durable history");
        let payload_start = FILE_HEADER_LEN + FRAME_PREFIX_LEN;
        assert!(bytes.len() > payload_start);
        bytes[payload_start] ^= 0x01;
        fs::write(temp.path(), bytes).expect("write corrupted history");

        let error = FileCommitStore::open(temp.path()).expect_err("corruption must fail closed");
        assert!(matches!(error, FileCommitStoreError::CorruptFrame { .. }));
    }

    #[test]
    fn identical_histories_have_identical_canonical_bytes() {
        let first = TempFile::new("canonical-a");
        let second = TempFile::new("canonical-b");
        write_one_world(first.path());
        write_one_world(second.path());
        assert_eq!(
            fs::read(first.path()).expect("read first history"),
            fs::read(second.path()).expect("read second history")
        );
    }

    #[test]
    fn invalid_file_header_fails_closed() {
        let temp = TempFile::new("header");
        fs::write(temp.path(), b"not-a-forthdb-file").expect("write invalid file");
        let error = FileCommitStore::open(temp.path()).expect_err("invalid header must fail");
        assert!(matches!(error, FileCommitStoreError::InvalidHeader(_)));
    }
}
