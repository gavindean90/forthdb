#![cfg(target_os = "linux")]

use forthdb_core::{Atom, EntityId, Fact, Literal, Predicate, SlotId};
use forthdb_world::{
    CommitStore, DurableControllerOpenError, DurableControllerState, DurableQueuedIntentController,
    FileCommitStore, FileEpochSyncPolicy, QueuedIntent, WriterLeaseError, writer_lock_path,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const CHILD_MODE: &str = "FORTHDB_M6D_CHILD_MODE";
const CHILD_DATABASE: &str = "FORTHDB_M6D_CHILD_DATABASE";
const CHILD_READY: &str = "FORTHDB_M6D_CHILD_READY";
const CRASH_POINT: &str = "FORTHDB_M6D_CRASH_POINT";
const CRASH_EXIT_CODE: i32 = 86;

static PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn temp_path(label: &str) -> PathBuf {
    let sequence = PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "forthdb-m6d-process-{label}-{}-{sequence}.db",
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

fn intent(slot: &str, value: &str) -> QueuedIntent {
    let mut intent = QueuedIntent::new();
    intent.define_fact(SlotId::new(slot), state_fact(value));
    intent
}

fn spawn_child(mode: &str, database: &Path, ready: Option<&Path>, crash: Option<&str>) -> Child {
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command
        .arg("--exact")
        .arg("child_process_entry")
        .arg("--nocapture")
        .env(CHILD_MODE, mode)
        .env(CHILD_DATABASE, database);
    if let Some(ready) = ready {
        command.env(CHILD_READY, ready);
    }
    if let Some(crash) = crash {
        command.env(CRASH_POINT, crash);
    }
    command.spawn().expect("child process starts")
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(feature = "fault-injection")]
fn assert_crash_status(status: ExitStatus) {
    assert_eq!(
        status.code(),
        Some(CRASH_EXIT_CODE),
        "fault-injected child exited unexpectedly: {status:?}"
    );
}

fn cleanup(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(writer_lock_path(path));
}

#[test]
fn child_process_entry() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    let database = PathBuf::from(std::env::var_os(CHILD_DATABASE).expect("child database path"));
    match mode.as_str() {
        "hold-writer-lock" => {
            let _controller = DurableQueuedIntentController::open_owned(
                &database,
                FileEpochSyncPolicy::PerEpoch,
                16,
                8,
            )
            .expect("child acquires writer ownership");
            let ready = PathBuf::from(std::env::var_os(CHILD_READY).expect("child ready path"));
            fs::write(ready, b"ready").expect("child publishes readiness");
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
        #[cfg(feature = "fault-injection")]
        "crash-one-epoch" => {
            let controller = DurableQueuedIntentController::open_owned(
                &database,
                FileEpochSyncPolicy::PerEpoch,
                16,
                8,
            )
            .expect("child acquires writer ownership");
            let ticket = controller
                .submit(intent("process/crash", "durable"))
                .expect("child admits intent");
            let _ = ticket.wait();
            panic!("configured lifecycle crash point was not reached");
        }
        other => panic!("unknown child mode {other}"),
    }
}

#[test]
fn writer_lease_is_exclusive_across_processes_and_released_after_sigkill() {
    let database = temp_path("writer-lock");
    let ready = database.with_extension("ready");
    let mut child = spawn_child("hold-writer-lock", &database, Some(&ready), None);
    wait_for_file(&ready);

    let second =
        DurableQueuedIntentController::open_owned(&database, FileEpochSyncPolicy::PerEpoch, 16, 8);
    assert!(matches!(
        second,
        Err(DurableControllerOpenError::WriterLease(
            WriterLeaseError::AlreadyHeld(_)
        ))
    ));

    let kill_result = unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    assert_eq!(kill_result, 0, "SIGKILL succeeds");
    let status = child.wait().expect("killed child is reaped");
    assert!(!status.success());

    let controller =
        DurableQueuedIntentController::open_owned(&database, FileEpochSyncPolicy::PerEpoch, 16, 8)
            .expect("writer ownership is released by process death");
    let report = controller.shutdown();
    assert_eq!(report.final_state, DurableControllerState::Closed);

    let _ = fs::remove_file(ready);
    cleanup(&database);
}

#[cfg(feature = "fault-injection")]
#[test]
fn crash_windows_recover_only_the_physically_durable_prefix() {
    let cases = [
        ("after_derive_before_persist", 0usize),
        ("after_persist_before_publish", 1usize),
        ("after_publish_before_delivery", 1usize),
    ];

    for (point, expected_frames) in cases {
        let database = temp_path(point);
        let mut child = spawn_child("crash-one-epoch", &database, None, Some(point));
        assert_crash_status(child.wait().expect("crash child is reaped"));

        let recovered = FileCommitStore::open(&database).expect("cold recovery succeeds");
        assert_eq!(
            recovered.len(),
            expected_frames,
            "unexpected recovered prefix at crash point {point}"
        );
        drop(recovered);

        let controller = DurableQueuedIntentController::open_owned(
            &database,
            FileEpochSyncPolicy::PerEpoch,
            16,
            8,
        )
        .expect("writer can restart after crash");
        let expected_version = expected_frames as u64;
        assert_eq!(controller.database().snapshot().version(), expected_version);
        let ticket = controller
            .submit(intent("process/restart", point))
            .expect("post-crash intent is admitted");
        let committed = ticket
            .wait()
            .expect("post-crash ticket resolves")
            .world()
            .expect("post-crash intent commits");
        assert_eq!(committed.version(), expected_version + 1);
        assert_eq!(
            controller.shutdown().final_state,
            DurableControllerState::Closed
        );

        cleanup(&database);
    }
}
