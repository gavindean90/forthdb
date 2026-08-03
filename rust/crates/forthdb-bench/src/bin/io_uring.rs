#[cfg(target_os = "linux")]
mod linux {
    use forthdb_core::{Atom, Fact, Literal, Predicate, SlotId};
    use forthdb_world::{CommitStore, Database, FileCommitStore, IoUringCommitStore};
    use serde::Serialize;
    use std::env;
    use std::fs;
    use std::hint::black_box;
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
        availability_error: Option<String>,
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
                "forthdb-bench-io-uring-{name}-{}-{sequence}.log",
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

    pub fn main() {
        let started = Instant::now();
        let availability_error = probe_io_uring().err().map(|error| error.to_string());
        let measurements = if availability_error.is_none() {
            vec![
                file_noop_sequence_measurement(100),
                io_uring_noop_sequence_measurement(100),
                file_define_sequence_measurement(100),
                io_uring_define_sequence_measurement(100),
                io_uring_noop_sequence_measurement(1_000),
                io_uring_open_existing_measurement(1_000),
            ]
        } else {
            Vec::new()
        };

        let report = BenchmarkReport {
            implementation: "rust",
            scope: "io-uring-commit-store",
            status: if availability_error.is_none() {
                "observational"
            } else {
                "unavailable"
            },
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
            availability_error,
            total_elapsed_ms: duration_millis(started.elapsed()),
        };

        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("benchmark report must serialize")
        );
    }

    fn probe_io_uring() -> Result<(), forthdb_world::IoUringCommitStoreError> {
        let temp = TempFile::new("probe");
        let store = IoUringCommitStore::open(temp.path())?;
        assert_eq!(store.max_in_flight_commits(), 1);
        assert_eq!(store.ring_entries(), 2);
        Ok(())
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

    fn file_noop_sequence_measurement(commits: u64) -> Measurement {
        measure_with_setup(
            "file_store_individually_synced_noop_commits_100",
            "commit",
            commits,
            || {
                let temp = TempFile::new("file-noop");
                let store = FileCommitStore::open(temp.path()).expect("file store opens");
                let database = Database::new(store).expect("empty history is valid");
                (temp, database)
            },
            move |(temp, database)| {
                for _ in 0..commits {
                    database
                        .commit(database.begin())
                        .expect("ordinary durable commit succeeds");
                }
                database.snapshot().version()
                    ^ fs::metadata(temp.path()).expect("metadata exists").len()
            },
            "Same-run baseline: canonical write_all, sync_data, and publication for each fixed-size frame.",
        )
    }

    fn io_uring_noop_sequence_measurement(commits: u64) -> Measurement {
        let (name, notes) = match commits {
            100 => (
                "io_uring_linked_write_fsync_noop_commits_100",
                "One persistent ring, one commit in flight, linked write and DATASYNC fsync completions before each publication.",
            ),
            1_000 => (
                "io_uring_linked_write_fsync_noop_commits_1000",
                "Longer steady-state sequence using one persistent two-entry ring and one commit in flight.",
            ),
            _ => (
                "io_uring_linked_write_fsync_noop_commits",
                "One persistent ring and one individually synchronized commit in flight.",
            ),
        };
        measure_with_setup(
            name,
            "commit",
            commits,
            || {
                let temp = TempFile::new("ring-noop");
                let store = IoUringCommitStore::open(temp.path()).expect("io_uring store opens");
                let database = Database::new(store).expect("empty history is valid");
                (temp, database)
            },
            move |(temp, database)| {
                for _ in 0..commits {
                    database
                        .commit(database.begin())
                        .expect("io_uring durable commit succeeds");
                }
                database.snapshot().version()
                    ^ fs::metadata(temp.path()).expect("metadata exists").len()
            },
            notes,
        )
    }

    fn file_define_sequence_measurement(commits: u64) -> Measurement {
        measure_with_setup(
            "file_store_individually_synced_define_commits_100",
            "commit",
            commits,
            || {
                let temp = TempFile::new("file-define");
                let store = FileCommitStore::open(temp.path()).expect("file store opens");
                let database = Database::new(store).expect("empty history is valid");
                (temp, database)
            },
            move |(temp, database)| {
                commit_definitions(database, commits);
                database.snapshot().record_count() as u64
                    ^ fs::metadata(temp.path()).expect("metadata exists").len()
            },
            "Same-run growing-world baseline using ordinary synchronized append.",
        )
    }

    fn io_uring_define_sequence_measurement(commits: u64) -> Measurement {
        measure_with_setup(
            "io_uring_linked_write_fsync_define_commits_100",
            "commit",
            commits,
            || {
                let temp = TempFile::new("ring-define");
                let store = IoUringCommitStore::open(temp.path()).expect("io_uring store opens");
                let database = Database::new(store).expect("empty history is valid");
                (temp, database)
            },
            move |(temp, database)| {
                commit_definitions(database, commits);
                database.snapshot().record_count() as u64
                    ^ fs::metadata(temp.path()).expect("metadata exists").len()
            },
            "Growing deep-cloned world plus one linked write and data-sync pair per publication.",
        )
    }

    fn io_uring_open_existing_measurement(frame_count: u64) -> Measurement {
        measure_with_setup(
            "io_uring_open_existing_1000_frames",
            "open",
            1,
            move || prepare_noop_file(frame_count),
            |temp| {
                let store = IoUringCommitStore::open(temp.path()).expect("io_uring store reopens");
                store.len() as u64
                    ^ store.file_len().expect("file length")
                    ^ store.ring_entries() as u64
            },
            "Includes established FileCommitStore recovery and validation, reopening the file descriptor, and constructing a two-entry ring.",
        )
    }

    fn commit_definitions<S: forthdb_world::CommitStore>(database: &Database<S>, commits: u64) {
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
    }

    fn prepare_noop_file(frame_count: u64) -> TempFile {
        let temp = TempFile::new("open-existing");
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
}

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!(
        "{{\"implementation\":\"rust\",\"scope\":\"io-uring-commit-store\",\"status\":\"unavailable\",\"measurements\":[],\"availability_error\":\"IoUringCommitStore is supported only on Linux\"}}"
    );
}
