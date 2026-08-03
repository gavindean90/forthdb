use forthdb_core::{Atom, EntityId, Fact, Literal, Predicate, SlotId};
use forthdb_world::{
    CommitFrame, CommitStore, Database, EpochFileIo, EpochIoPhase, FileCommitStore, FileEpochState,
    FileEpochStore, FileEpochStoreError, FileEpochSyncPolicy, MemoryCommitStore, StdEpochFileIo,
};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const HEADER_LEN: usize = 16;
const ENOSPC: i32 = 28;
static PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct PrefixThenFailIo {
    inner: StdEpochFileIo,
    prefix: usize,
    failed: AtomicBool,
}

impl PrefixThenFailIo {
    fn open(path: &Path, prefix: usize) -> Self {
        Self {
            inner: StdEpochFileIo::open(path).expect("fault file opens"),
            prefix,
            failed: AtomicBool::new(false),
        }
    }
}

impl EpochFileIo for PrefixThenFailIo {
    fn len(&mut self, phase: EpochIoPhase) -> std::io::Result<u64> {
        self.inner.len(phase)
    }

    fn write_at(
        &mut self,
        phase: EpochIoPhase,
        offset: u64,
        bytes: &[u8],
    ) -> std::io::Result<usize> {
        if phase == EpochIoPhase::EpochWrite && !self.failed.swap(true, Ordering::SeqCst) {
            let target = self.prefix.min(bytes.len());
            let mut written = 0usize;
            while written < target {
                let count =
                    self.inner
                        .write_at(phase, offset + written as u64, &bytes[written..target])?;
                if count == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "injected prefix write made no progress",
                    ));
                }
                written += count;
            }
            return Err(std::io::Error::from_raw_os_error(ENOSPC));
        }
        self.inner.write_at(phase, offset, bytes)
    }

    fn sync_data(&mut self, phase: EpochIoPhase) -> std::io::Result<()> {
        self.inner.sync_data(phase)
    }

    fn set_len(&mut self, phase: EpochIoPhase, len: u64) -> std::io::Result<()> {
        self.inner.set_len(phase, len)
    }

    fn read_all(&mut self, phase: EpochIoPhase) -> std::io::Result<Vec<u8>> {
        self.inner.read_all(phase)
    }
}

fn temp_path(label: &str) -> PathBuf {
    let sequence = PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "forthdb-m6b-fault-{label}-{}-{sequence}.db",
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

fn frames(count: usize) -> Vec<Arc<CommitFrame>> {
    let database = Database::new(MemoryCommitStore::new()).expect("memory database opens");
    let mut first = database.begin();
    assert_eq!(first.entity(), EntityId::new(1));
    first.define(SlotId::new("state/0"), state_fact("zero"));
    database.commit(first).expect("first commits");
    for index in 1..count {
        let mut transaction = database.begin();
        transaction.define(
            SlotId::new(format!("state/{index}")),
            state_fact(&index.to_string()),
        );
        database.commit(transaction).expect("frame commits");
    }
    database.frames()
}

#[test]
fn every_epoch_byte_boundary_repairs_to_the_exact_prefix() {
    let frames = frames(3);
    let complete_path = temp_path("complete");
    let mut complete = FileEpochStore::open(&complete_path, FileEpochSyncPolicy::PerEpoch)
        .expect("complete store opens");
    complete
        .append_epoch(&frames)
        .expect("complete epoch commits");
    drop(complete);
    let complete_bytes = fs::read(&complete_path).expect("complete bytes read");
    let arena_len = complete_bytes.len() - HEADER_LEN;
    let _ = fs::remove_file(&complete_path);

    for prefix in 0..=arena_len {
        let path = temp_path("boundary");
        FileCommitStore::open(&path).expect("file initializes");
        let checkpoint = fs::read(&path).expect("checkpoint reads");
        let io = PrefixThenFailIo::open(&path, prefix);
        let mut store = FileEpochStore::from_io(&path, io, FileEpochSyncPolicy::PerEpoch)
            .expect("fault store opens");
        let error = store
            .append_epoch(&frames)
            .expect_err("injected write must fail");
        assert!(matches!(error, FileEpochStoreError::EpochRepaired { .. }));
        assert_eq!(store.state(), FileEpochState::Healthy);
        assert_eq!(store.len(), 0);
        assert_eq!(fs::read(&path).expect("repaired bytes read"), checkpoint);
        let reopened = FileCommitStore::open(&path).expect("repaired file reopens");
        assert_eq!(reopened.len(), 0);
        drop(reopened);
        let _ = fs::remove_file(path);
    }
}

#[test]
fn crash_writer_child() {
    let Ok(target) = std::env::var("FORTHDB_M6B_CRASH_TARGET") else {
        return;
    };
    let source = std::env::var("FORTHDB_M6B_CRASH_SOURCE").expect("source path supplied");
    let start: usize = std::env::var("FORTHDB_M6B_CRASH_START")
        .expect("start supplied")
        .parse()
        .expect("start parses");
    let count: usize = std::env::var("FORTHDB_M6B_CRASH_COUNT")
        .expect("count supplied")
        .parse()
        .expect("count parses");
    let source_bytes = fs::read(source).expect("source bytes read");
    let mut file = OpenOptions::new()
        .append(true)
        .open(target)
        .expect("crash target opens");
    file.write_all(&source_bytes[start..start + count])
        .expect("crash bytes write");
    file.flush().expect("userspace buffer flushes");
    std::process::exit(97);
}

fn run_crash_case(full_second_frame: bool) {
    let frames = frames(2);
    let one_path = temp_path("crash-one");
    let full_path = temp_path("crash-full");
    let crash_path = temp_path("crash-target");

    let mut one = FileEpochStore::open(&one_path, FileEpochSyncPolicy::PerEpoch)
        .expect("one-frame store opens");
    one.append_epoch(&frames[..1]).expect("first frame commits");
    drop(one);

    let mut full = FileEpochStore::open(&full_path, FileEpochSyncPolicy::PerEpoch)
        .expect("two-frame store opens");
    full.append_epoch(&frames).expect("two frames commit");
    drop(full);

    fs::copy(&one_path, &crash_path).expect("known-good prefix copies");
    let good_len = fs::metadata(&one_path).expect("good metadata").len() as usize;
    let full_len = fs::metadata(&full_path).expect("full metadata").len() as usize;
    let second_len = full_len - good_len;
    let count = if full_second_frame {
        second_len
    } else {
        (second_len / 2).max(1)
    };

    let status = Command::new(std::env::current_exe().expect("test executable path"))
        .arg("--exact")
        .arg("crash_writer_child")
        .arg("--nocapture")
        .env("FORTHDB_M6B_CRASH_TARGET", &crash_path)
        .env("FORTHDB_M6B_CRASH_SOURCE", &full_path)
        .env("FORTHDB_M6B_CRASH_START", good_len.to_string())
        .env("FORTHDB_M6B_CRASH_COUNT", count.to_string())
        .status()
        .expect("crash child launches");
    assert_eq!(status.code(), Some(97));

    let recovered = FileCommitStore::open(&crash_path).expect("crash file recovers");
    if full_second_frame {
        assert_eq!(recovered.len(), 2);
        assert_eq!(recovered.recovered_tail_bytes(), 0);
    } else {
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered.recovered_tail_bytes(), count as u64);
    }
    drop(recovered);

    let _ = fs::remove_file(one_path);
    let _ = fs::remove_file(full_path);
    let _ = fs::remove_file(crash_path);
}

#[test]
fn process_crash_after_partial_frame_recovers_only_sound_prefix() {
    run_crash_case(false);
}

#[test]
fn process_crash_after_complete_frame_recovers_the_complete_frame() {
    run_crash_case(true);
}
