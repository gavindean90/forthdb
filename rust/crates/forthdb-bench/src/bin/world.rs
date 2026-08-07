use forthdb_core::{Atom, Fact, Literal, Predicate, SlotId};
use forthdb_world::{CommitStore, Database, MemoryCommitStore, Transaction};
use serde::Serialize;
use std::env;
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SAMPLES: usize = 3;

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
    name: String,
    unit: &'static str,
    operations_per_sample: u64,
    samples: usize,
    median_ns_per_operation: f64,
    min_ns_per_operation: f64,
    max_ns_per_operation: f64,
    median_operations_per_second: f64,
    sample_elapsed_ns: Vec<u64>,
    checksum: u64,
    notes: String,
}

fn main() {
    let started = Instant::now();
    let measurements = vec![
        candidate_size_measurement(1, 10_000),
        candidate_size_measurement(10, 5_000),
        candidate_size_measurement(100, 500),
        candidate_size_measurement(1_000, 50),
        candidate_history_measurement(100, 2_000),
        candidate_history_measurement(1_000, 200),
        candidate_history_measurement(10_000, 10),
        snapshot_capture_measurement(1_000_000),
        commit_sequence_measurement(1_000),
        reconstruction_measurement(1_000, 20),
    ];

    let report = BenchmarkReport {
        implementation: "rust",
        scope: "memory-committed-world-engine",
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

fn candidate_size_measurement(operation_count: u64, iterations: u64) -> Measurement {
    measure_with_setup(
        format!("candidate_construct_operations_{operation_count}"),
        "candidate",
        iterations,
        move || transaction_with_definitions(operation_count),
        move |transaction| {
            let mut checksum = 0_u64;
            for _ in 0..iterations {
                let candidate = black_box(
                    transaction
                        .candidate()
                        .expect("benchmark candidate must construct"),
                );
                checksum = checksum
                    .wrapping_add(candidate.id().value())
                    .wrapping_add(candidate.record_count() as u64);
            }
            checksum
        },
        format!(
            "Clones an empty immutable base, applies {operation_count} staged definitions, and kernel-validates the private successor."
        ),
    )
}

fn candidate_history_measurement(history_depth: u64, iterations: u64) -> Measurement {
    measure_with_setup(
        format!("candidate_construct_one_operation_history_{history_depth}"),
        "candidate",
        iterations,
        move || transaction_on_history(history_depth),
        move |transaction| {
            let mut checksum = 0_u64;
            for _ in 0..iterations {
                let candidate = black_box(
                    transaction
                        .candidate()
                        .expect("benchmark candidate must construct"),
                );
                checksum = checksum
                    .wrapping_add(candidate.id().value())
                    .wrapping_add(candidate.record_count() as u64);
            }
            checksum
        },
        format!(
            "Deep-clones and validates a base world containing {history_depth} committed definitions, then applies one staged definition."
        ),
    )
}

fn snapshot_capture_measurement(iterations: u64) -> Measurement {
    measure_with_setup(
        "snapshot_capture".to_owned(),
        "snapshot",
        iterations,
        || {
            let database =
                Database::new(MemoryCommitStore::new()).expect("empty memory store is valid");
            let mut transaction = database.begin();
            transaction.define(
                SlotId::new("snapshot/state"),
                literal_fact("snapshot", "state", "ready"),
            );
            database
                .commit(transaction)
                .expect("snapshot benchmark setup commits");
            database
        },
        move |database| {
            let mut checksum = 0_u64;
            for _ in 0..iterations {
                let snapshot = black_box(database.snapshot());
                checksum = checksum.wrapping_add(snapshot.id().value());
            }
            checksum
        },
        "Captures an immutable reader world through the current-world read lock and clones its Arc.".to_owned(),
    )
}

fn commit_sequence_measurement(commit_count: u64) -> Measurement {
    measure_with_setup(
        format!("commit_sequence_one_operation_{commit_count}"),
        "commit",
        commit_count,
        || Database::new(MemoryCommitStore::new()).expect("empty memory store is valid"),
        move |database| {
            let mut checksum = 0_u64;
            for index in 0..commit_count {
                let mut transaction = database.begin();
                transaction.define(
                    SlotId::new(format!("commit/{index}")),
                    literal_fact("commit", "value", &index.to_string()),
                );
                let world = black_box(
                    database
                        .commit(transaction)
                        .expect("benchmark commit must succeed"),
                );
                checksum = checksum
                    .wrapping_add(world.version())
                    .wrapping_add(world.id().value());
            }
            checksum
        },
        "End-to-end in-memory commit: base-world clone, staged operation, kernel validation, frame append, and atomic publication while history grows.".to_owned(),
    )
}

fn reconstruction_measurement(frame_count: u64, iterations: u64) -> Measurement {
    measure_with_setup(
        format!("reconstruct_memory_store_frames_{frame_count}"),
        "reconstruction",
        iterations,
        move || committed_frames(frame_count),
        move |frames| {
            let mut checksum = 0_u64;
            for _ in 0..iterations {
                let mut store = MemoryCommitStore::new();
                for frame in frames.iter() {
                    store
                        .append(frame.clone())
                        .expect("memory append is infallible");
                }
                let database =
                    black_box(Database::new(store).expect("benchmark history must reconstruct"));
                let world = database.snapshot();
                checksum = checksum
                    .wrapping_add(world.id().value())
                    .wrapping_add(world.record_count() as u64);
            }
            checksum
        },
        format!(
            "Enumerates and verifies {frame_count} no-op commit frames into a fresh in-memory world. This is logical reconstruction, not filesystem recovery."
        ),
    )
}

fn transaction_with_definitions(operation_count: u64) -> Transaction {
    let database = Database::new(MemoryCommitStore::new()).expect("empty memory store is valid");
    let mut transaction = database.begin();
    for index in 0..operation_count {
        transaction.define(
            SlotId::new(format!("candidate/{index}")),
            literal_fact("candidate", "value", &index.to_string()),
        );
    }
    transaction
}

fn transaction_on_history(history_depth: u64) -> Transaction {
    let database = Database::new(MemoryCommitStore::new()).expect("empty memory store is valid");
    let mut history = database.begin();
    for index in 0..history_depth {
        history.define(
            SlotId::new(format!("history/{index}")),
            literal_fact("history", "value", &index.to_string()),
        );
    }
    database
        .commit(history)
        .expect("history setup commit must succeed");

    let mut transaction = database.begin();
    transaction.define(
        SlotId::new("history/new"),
        literal_fact("history", "value", "new"),
    );
    transaction
}

fn committed_frames(frame_count: u64) -> Vec<Arc<forthdb_world::CommitFrame>> {
    let database = Database::new(MemoryCommitStore::new()).expect("empty memory store is valid");
    for _ in 0..frame_count {
        database
            .commit(database.begin())
            .expect("no-op frame setup commit must succeed");
    }
    database.frames()
}

fn literal_fact(subject: &str, predicate: &str, object: &str) -> Fact {
    Fact::new(
        Atom::Literal(Literal::new(subject)),
        Predicate::new(predicate),
        Atom::Literal(Literal::new(object)),
    )
}

fn measure_with_setup<State, Setup, Run>(
    name: String,
    unit: &'static str,
    operations_per_sample: u64,
    mut setup: Setup,
    mut run: Run,
    notes: String,
) -> Measurement
where
    Setup: FnMut() -> State,
    Run: FnMut(&mut State) -> u64,
{
    let mut elapsed = Vec::with_capacity(SAMPLES);
    let mut checksum = 0_u64;
    for _ in 0..SAMPLES {
        forthdb_core::ForthDb::drain_reaper(Duration::from_secs(5));
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

fn duration_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
