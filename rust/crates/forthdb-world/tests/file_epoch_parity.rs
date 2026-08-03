use forthdb_core::{Atom, EntityId, Fact, Literal, Predicate, SlotId};
use forthdb_world::{
    CommitStore, Database, FileCommitStore, FileEpochStore, FileEpochSyncPolicy,
    MemoryCommitStore,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn temp_path(label: &str) -> PathBuf {
    let sequence = PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "forthdb-file-epoch-parity-{label}-{}-{sequence}.db",
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

#[test]
fn both_epoch_policies_match_the_established_file_store_bytes() {
    let memory = Database::new(MemoryCommitStore::new()).expect("memory database opens");
    let mut first = memory.begin();
    assert_eq!(first.entity(), EntityId::new(1));
    first.define(SlotId::new("state/one"), state_fact("one"));
    memory.commit(first).expect("first frame commits");
    for value in ["two", "three", "four"] {
        let mut transaction = memory.begin();
        transaction.define(
            SlotId::new(format!("state/{value}")),
            state_fact(value),
        );
        memory.commit(transaction).expect("frame commits");
    }
    let frames = memory.frames();

    let oracle_path = temp_path("oracle");
    let per_frame_path = temp_path("per-frame");
    let per_epoch_path = temp_path("per-epoch");

    let mut oracle = FileCommitStore::open(&oracle_path).expect("oracle opens");
    for frame in &frames {
        oracle.append(frame.clone()).expect("oracle frame appends");
    }
    drop(oracle);

    let mut per_frame =
        FileEpochStore::open(&per_frame_path, FileEpochSyncPolicy::PerFrame)
            .expect("per-frame store opens");
    per_frame
        .append_epoch(&frames)
        .expect("per-frame epoch appends");
    drop(per_frame);

    let mut per_epoch =
        FileEpochStore::open(&per_epoch_path, FileEpochSyncPolicy::PerEpoch)
            .expect("per-epoch store opens");
    per_epoch
        .append_epoch(&frames)
        .expect("per-epoch arena appends");
    drop(per_epoch);

    let oracle_bytes = fs::read(&oracle_path).expect("oracle bytes read");
    assert_eq!(
        fs::read(&per_frame_path).expect("per-frame bytes read"),
        oracle_bytes
    );
    assert_eq!(
        fs::read(&per_epoch_path).expect("per-epoch bytes read"),
        oracle_bytes
    );

    let oracle_reopened = FileCommitStore::open(&oracle_path).expect("oracle reopens");
    let frame_reopened = FileCommitStore::open(&per_frame_path).expect("per-frame reopens");
    let epoch_reopened = FileCommitStore::open(&per_epoch_path).expect("per-epoch reopens");
    assert_eq!(oracle_reopened.frames(), frames);
    assert_eq!(frame_reopened.frames(), frames);
    assert_eq!(epoch_reopened.frames(), frames);

    drop(oracle_reopened);
    drop(frame_reopened);
    drop(epoch_reopened);
    let _ = fs::remove_file(oracle_path);
    let _ = fs::remove_file(per_frame_path);
    let _ = fs::remove_file(per_epoch_path);
}
