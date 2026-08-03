use forthdb_world::{CommitStore, Database, FileCommitStore, MmapCommitStore};
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
            "forthdb-mmap-bench-{name}-{}-{sequence}.log",
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
        mmap_open_measurement(100),
        mmap_open_measurement(1_000),
        mmap_open_measurement(10_000),
        paired_file_open_measurement(1_000),
        mapped_byte_scan_measurement(10_000, 1_000),
        mmap_durable_noop_sequence_measurement(100),
        incomplete_tail_recovery_measurement(100),
    ];

    let report = BenchmarkReport {
        implementation: "rust",
        scope: "mmap-commit-store",
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

fn mmap_open_measurement(frame_count: u64) -> Measurement {
    let name = match frame_count {
        100 => "mmap_open_and_reconstruct_100_frames",
        1_000 => "mmap_open_and_reconstruct_1000_frames",
        10_000 => "mmap_open_and_reconstruct_10000_frames",
        _ => "mmap_open_and_reconstruct_frames",
    };
    measure_with_setup(
        name,
        "open",
        1,
        move || prepare_noop_file(frame_count, "mmap-open"),
        |temp| {
            let store = MmapCommitStore::open(temp.path()).expect("mmap history opens");
            let mapped = store.mapped_len() as u64;
            let frames = store.len() as u64;
            let database = Database::new(store).expect("mapped history reconstructs");
            mapped ^ frames ^ database.snapshot().version()
        },
        "Includes mmap creation, mapped frame scan, checksum verification, payload decoding, store validation, and database reconstruction.",
    )
}

fn paired_file_open_measurement(frame_count: u64) -> Measurement {
    measure_with_setup(
        "file_open_and_reconstruct_1000_frames_paired",
        "open",
        1,
        move || prepare_noop_file(frame_count, "file-open-paired"),
        |temp| {
            let store = FileCommitStore::open(temp.path()).expect("file history opens");
            let frames = store.len() as u64;
            let database = Database::new(store).expect("file history reconstructs");
            frames ^ database.snapshot().version()
        },
        "Same generated v1 file and hosted run as the mmap measurement, using read_to_end instead of mapping.",
    )
}

fn mapped_byte_scan_measurement(frame_count: u64, scans: u64) -> Measurement {
    measure_with_setup(
        "mapped_full_byte_scan_10000_frames",
        "scan",
        scans,
        move || {
            let temp = prepare_noop_file(frame_count, "byte-scan");
            let store = MmapCommitStore::open(temp.path()).expect("mmap history opens");
            (temp, store)
        },
        move |(_temp, store)| {
            let bytes = store.mapped_bytes().expect("mapping is available");
            let mut total = 0_u64;
            for _ in 0..scans {
                let mut sample = 0_u64;
                for byte in bytes {
                    sample = sample.wrapping_add(u64::from(*byte));
                }
                total = total.wrapping_add(black_box(sample));
            }
            total
        },
        "Sequentially consumes every mapped byte without copying the file into an owned input buffer.",
    )
}

fn mmap_durable_noop_sequence_measurement(commits: u64) -> Measurement {
    measure_with_setup(
        "mmap_durable_noop_commit_sequence_100",
        "commit",
        commits,
        || {
            let temp = TempFile::new("durable-sequence");
            let store = MmapCommitStore::open(temp.path()).expect("mmap store opens");
            let database = Database::new(store).expect("empty history is valid");
            database
                .commit(database.begin())
                .expect("warm writer and mapping");
            (temp, database)
        },
        move |(temp, database)| {
            for _ in 0..commits {
                database
                    .commit(database.begin())
                    .expect("mapped durable commit succeeds");
            }
            database.snapshot().version()
                ^ fs::metadata(temp.path())
                    .expect("benchmark file metadata")
                    .len()
        },
        "Steady-state synchronized no-op commits after lazy writer initialization; each successful append refreshes the mapping.",
    )
}

fn incomplete_tail_recovery_measurement(frame_count: u64) -> Measurement {
    measure_with_setup(
        "mmap_recover_incomplete_tail_after_100_frames",
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
            let store = MmapCommitStore::open(temp.path()).expect("incomplete tail recovers");
            store.recovered_tail_bytes() ^ store.mapped_len() as u64 ^ store.len() as u64
        },
        "Includes mmap scan, detection of a seven-byte incomplete record, unmap, truncation, sync_data, and remap.",
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
