use forthdb_core::{Atom, Fact, Literal, Predicate, SlotId};
use forthdb_world::{CommitStore, Database, FileCommitStore};
use serde::Serialize;
use std::env;
use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SAMPLES: usize = 3;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    implementation: &'static str,
    scope: &'static str,
    status: &'static str,
    profile: &'static str,
    environment: Environment,
    measurements: Vec<Measurement>,
    total_elapsed_ms: u64,
}

#[derive(Debug, Serialize)]
struct Environment {
    os: &'static str,
    architecture: &'static str,
    crate_version: &'static str,
    git_sha: Option<String>,
    github_run_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct Measurement {
    name: &'static str,
    unit: &'static str,
    operations_per_sample: u64,
    samples: usize,
    median_ns_per_operation: f64,
    min_ns_per_operation: f64,
    max_ns_per_operation: f64,
    median_operations_per_second: f64,
    sample_elapsed_ns: Vec<u64>,
    checksum: u64,
    notes: &'static str,
}

struct TempFile(PathBuf);

impl TempFile {
    fn new(name: &str) -> Self {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "forthdb-bench-{name}-{}-{sequence}.log",
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

fn main() {
    let started = Instant::now();
    let measurements = vec![
        durable_noop_sequence_measurement(100),
        durable_define_sequence_measurement(100),
        reopen_measurement(100),
        reopen_measurement(1_000),
        incomplete_tail_recovery_measurement(100),
    ];

    let report = BenchmarkReport {
        implementation: "rust",
        scope: "file-commit-store",
        status: "observational",
        profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        environment: Environment {
            os: env::consts::OS,
            architecture: env::consts::ARCH,
            crate_version: env!("CARGO_PKG_VERSION"),
            git_sha: env::var("GITHUB_SHA").ok(),
            github_run_id: env::var("GITHUB_RUN_ID").ok(),
        },
        measurements,
        total_elapsed_ms: duration_millis(started.elapsed()),
    };

    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("benchmark report must serialize")
    );
}

fn measure_with_setup<State, Setup, Run>(
    name: &'static str,
    unit: &'static str,
    operations_per_sample: u64,
    mut setup: Setup,
    mut run: Run,
    notes: &'static str,
) -> Measurement
where
    Setup: FnMut() -> State,
    Run: FnMut(&mut State) -> u64,
{
    let mut elapsed = Vec::with_capacity(SAMPLES);
    let mut checksum = 0_u64;
    for _ in 0..SAMPLES {
        let mut state = setup();
        let started = Instant::now();
        checksum = checksum.wrapping_add(black_box(run(&mut state)));
        elapsed.push(duration_nanos(started.elapsed()));
    }
    elapsed.sort_unstable();
    let ns_per_operation: Vec<f64> = elapsed
        .iter()
        .map(|nanos| *nanos as f64 / operations_per_sample as f64)
        .collect();
    let median = ns_per_operation[ns_per_operation.len() / 2];

    Measurement {
        name,
        unit,
        operations_per_sample,
        samples: SAMPLES,
        median_ns_per_operation: median,
        min_ns_per_operation: ns_per_operation[0],
        max_ns_per_operation: ns_per_operation[ns_per_operation.len() - 1],
        median_operations_per_second: 1_000_000_000.0 / median,
        sample_elapsed_ns: elapsed,
        checksum,
        notes,
    }
}

fn durable_noop_sequence_measurement(commits: u64) -> Measurement {
    measure_with_setup(
        "durable_noop_commit_sequence_100",
        "commit",
        commits,
        || {
            let temp = TempFile::new("noop-sequence");
            let store = FileCommitStore::open(temp.path()).expect("file store opens");
            let database = Database::new(store).expect("empty history is valid");
            (temp, database)
        },
        move |(temp, database)| {
            for _ in 0..commits {
                database
                    .commit(database.begin())
                    .expect("durable no-op commit succeeds");
            }
            database.snapshot().version()
                ^ fs::metadata(temp.path())
                    .expect("benchmark file metadata")
                    .len()
        },
        "Each commit constructs an empty candidate, appends one canonical frame, calls sync_data, and publishes.",
    )
}

fn durable_define_sequence_measurement(commits: u64) -> Measurement {
    measure_with_setup(
        "durable_define_commit_sequence_100",
        "commit",
        commits,
        || {
            let temp = TempFile::new("define-sequence");
            let store = FileCommitStore::open(temp.path()).expect("file store opens");
            let database = Database::new(store).expect("empty history is valid");
            (temp, database)
        },
        move |(temp, database)| {
            for index in 0..commits {
                let mut transaction = database.begin();
                transaction.define(
                    SlotId::new(format!("durable/value/{index}")),
                    Fact::new(
                        Atom::Literal(Literal::new("benchmark")),
                        Predicate::new("value"),
                        Atom::Literal(Literal::new(index.to_string())),
                    ),
                );
                database
                    .commit(transaction)
                    .expect("durable definition commit succeeds");
            }
            database.snapshot().record_count() as u64
                ^ fs::metadata(temp.path())
                    .expect("benchmark file metadata")
                    .len()
        },
        "Each commit deep-clones the growing world, applies one definition, validates, encodes, appends, syncs, and publishes.",
    )
}

fn reopen_measurement(frame_count: u64) -> Measurement {
    let name = match frame_count {
        100 => "reopen_and_reconstruct_100_frames",
        1_000 => "reopen_and_reconstruct_1000_frames",
        _ => "reopen_and_reconstruct_frames",
    };
    measure_with_setup(
        name,
        "reopen",
        1,
        move || prepare_noop_file(frame_count, "reopen"),
        |temp| {
            let store = FileCommitStore::open(temp.path()).expect("persisted file reopens");
            let frames = store.len() as u64;
            let database = Database::new(store).expect("persisted history reconstructs");
            frames ^ database.snapshot().version()
        },
        "Includes file read, header and frame validation, checksum verification, payload decoding, and logical world reconstruction.",
    )
}

fn incomplete_tail_recovery_measurement(frame_count: u64) -> Measurement {
    measure_with_setup(
        "recover_incomplete_tail_after_100_frames",
        "recovery",
        1,
        move || {
            let temp = prepare_noop_file(frame_count, "tail-recovery");
            let mut file = OpenOptions::new()
                .append(true)
                .open(temp.path())
                .expect("open benchmark tail");
            file.write_all(b"FRM1\x10\x00\x00")
                .expect("write incomplete tail");
            file.sync_data().expect("sync incomplete tail");
            temp
        },
        |temp| {
            let store = FileCommitStore::open(temp.path()).expect("incomplete tail recovers");
            store.recovered_tail_bytes() ^ store.len() as u64
        },
        "Includes validated reopen, detection of a seven-byte incomplete record, truncation to the last complete frame, and sync_data.",
    )
}

fn prepare_noop_file(frame_count: u64, name: &str) -> TempFile {
    let temp = TempFile::new(name);
    let store = FileCommitStore::open(temp.path()).expect("file store opens");
    let database = Database::new(store).expect("empty history is valid");
    for _ in 0..frame_count {
        database
            .commit(database.begin())
            .expect("durable setup commit succeeds");
    }
    drop(database);
    temp
}

fn duration_nanos(duration: Duration) -> u64 {
    duration.as_nanos().try_into().unwrap_or(u64::MAX)
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}
