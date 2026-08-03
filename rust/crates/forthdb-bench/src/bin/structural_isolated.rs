use forthdb_core::{Atom, Fact, ForthDb, LegacyForthDb, Literal, Predicate, ReaperMetrics, SlotId};
use forthdb_world::{Database, MemoryCommitStore, Transaction, World};
use serde::Serialize;
use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct CountingAllocator;

static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOCATION_COUNT: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, old, new_size) };
        if !replacement.is_null() && new_size > old.size() {
            ALLOCATED_BYTES.fetch_add((new_size - old.size()) as u64, Ordering::Relaxed);
        }
        replacement
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[derive(Clone, Copy)]
struct Counters {
    allocated_bytes: u64,
    allocations: u64,
}

impl Counters {
    fn capture() -> Self {
        Self {
            allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
            allocations: ALLOCATION_COUNT.load(Ordering::Relaxed),
        }
    }

    fn since(self, before: Self) -> Self {
        Self {
            allocated_bytes: self.allocated_bytes.saturating_sub(before.allocated_bytes),
            allocations: self.allocations.saturating_sub(before.allocations),
        }
    }
}

#[derive(Serialize)]
struct Report {
    implementation: &'static str,
    scope: &'static str,
    status: &'static str,
    profile: &'static str,
    environment: Environment,
    candidate_scaling: Vec<CandidateMeasurement>,
    legacy_control: Vec<CandidateMeasurement>,
    read_scaling: Vec<ReadMeasurement>,
    snapshot_retirement: SnapshotRetirement,
    total_elapsed_ms: u64,
}

#[derive(Serialize)]
struct Environment {
    os: &'static str,
    architecture: &'static str,
    crate_version: &'static str,
    git_sha: Option<String>,
    github_run_id: Option<String>,
}

#[derive(Serialize)]
struct CandidateMeasurement {
    name: String,
    retained_definitions: u64,
    delta_definitions: u64,
    samples: usize,
    iterations_per_sample: u64,
    median_ns_per_candidate: f64,
    min_ns_per_candidate: f64,
    max_ns_per_candidate: f64,
    median_allocated_bytes_per_candidate: f64,
    median_allocations_per_candidate: f64,
    checksum: u64,
    notes: String,
}

#[derive(Serialize)]
struct ReadMeasurement {
    retained_definitions: u64,
    iterations: u64,
    median_ns_per_resolve: f64,
    checksum: u64,
}

#[derive(Serialize)]
struct SnapshotRetirement {
    base_definitions: u64,
    snapshots: usize,
    p50_ns: u64,
    p95_ns: u64,
    p99_ns: u64,
    max_ns: u64,
    total_release_ns: u64,
    final_database_drop_ns: u64,
    reaper_drain_ns: u64,
    reaper_drained: bool,
    metrics_before: SerializableReaperMetrics,
    metrics_after: SerializableReaperMetrics,
    notes: &'static str,
}

#[derive(Serialize)]
struct SerializableReaperMetrics {
    queued_roots: usize,
    retired_roots: u64,
    reaped_roots: u64,
    overflow_enqueues: u64,
    worker_alive: bool,
}

impl From<ReaperMetrics> for SerializableReaperMetrics {
    fn from(value: ReaperMetrics) -> Self {
        Self {
            queued_roots: value.queued_roots,
            retired_roots: value.retired_roots,
            reaped_roots: value.reaped_roots,
            overflow_enqueues: value.overflow_enqueues,
            worker_alive: value.worker_alive,
        }
    }
}

struct PreparedWorld {
    transaction: Transaction,
    snapshot: Arc<World>,
    hot_slot: SlotId,
}

fn main() {
    let started = Instant::now();
    drain_reaper();
    let include_million = env::var("FORTHDB_M5_MILLION").ok().as_deref() == Some("1");
    let mut sizes = vec![100_u64, 1_000, 10_000, 100_000];
    if include_million {
        sizes.push(1_000_000);
    }

    let mut candidate_scaling = Vec::new();
    let mut read_scaling = Vec::new();
    for size in sizes {
        let prepared = prepare_world(size);
        drain_reaper();
        let (samples, iterations) = dimensions(size);
        candidate_scaling.push(measure_world_candidate(&prepared, size, samples, iterations));
        read_scaling.push(measure_resolve(&prepared, size));
        drop(prepared);
        drain_reaper();
    }

    drain_reaper();
    let legacy_control = [100_u64, 1_000, 10_000]
        .into_iter()
        .map(measure_legacy_clone)
        .collect();
    drain_reaper();

    let snapshot_retirement = measure_snapshot_retirement(100_000, 1_000);

    let report = Report {
        implementation: "rust",
        scope: "milestone-5-structural-sharing-isolated",
        status: "observational",
        profile: if cfg!(debug_assertions) { "debug" } else { "release" },
        environment: Environment {
            os: env::consts::OS,
            architecture: env::consts::ARCH,
            crate_version: env!("CARGO_PKG_VERSION"),
            git_sha: env::var("GITHUB_SHA").ok(),
            github_run_id: env::var("GITHUB_RUN_ID").ok(),
        },
        candidate_scaling,
        legacy_control,
        read_scaling,
        snapshot_retirement,
        total_elapsed_ms: duration_millis(started.elapsed()),
    };

    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("structural report serializes")
    );
}

fn drain_reaper() {
    assert!(
        ForthDb::drain_reaper(Duration::from_secs(60)),
        "background kernel retirement must drain before a timed sample"
    );
}

fn dimensions(size: u64) -> (usize, u64) {
    match size {
        0..=1_000 => (7, 50),
        1_001..=10_000 => (7, 20),
        10_001..=100_000 => (5, 5),
        _ => (5, 2),
    }
}

fn prepare_world(definitions: u64) -> PreparedWorld {
    let database = Database::new(MemoryCommitStore::new()).expect("memory database opens");
    let mut initial = database.begin();
    for index in 0..definitions {
        initial.define(
            SlotId::new(format!("retained/{index}")),
            literal_fact("retained", "value", &index.to_string()),
        );
    }
    database.commit(initial).expect("retained world commits");
    drain_reaper();
    let snapshot = database.snapshot();
    let hot_slot = SlotId::new(format!("retained/{}", definitions.saturating_sub(1)));
    let mut transaction = database.begin();
    transaction.define(
        SlotId::new("candidate/new"),
        literal_fact("candidate", "value", "new"),
    );
    PreparedWorld {
        transaction,
        snapshot,
        hot_slot,
    }
}

fn measure_world_candidate(
    prepared: &PreparedWorld,
    retained: u64,
    samples: usize,
    iterations: u64,
) -> CandidateMeasurement {
    let mut elapsed = Vec::with_capacity(samples);
    let mut allocated = Vec::with_capacity(samples);
    let mut allocations = Vec::with_capacity(samples);
    let mut checksum = 0_u64;

    for _ in 0..samples {
        drain_reaper();
        let before = Counters::capture();
        let started = Instant::now();
        let mut candidates = Vec::with_capacity(iterations as usize);
        for _ in 0..iterations {
            let candidate = black_box(
                prepared
                    .transaction
                    .candidate()
                    .expect("shared candidate constructs"),
            );
            checksum = checksum
                .wrapping_add(candidate.id().value())
                .wrapping_add(candidate.record_count() as u64);
            candidates.push(candidate);
        }
        elapsed.push(duration_nanos(started.elapsed()));
        let delta = Counters::capture().since(before);
        allocated.push(delta.allocated_bytes);
        allocations.push(delta.allocations);
        drop(candidates);
        drain_reaper();
    }

    elapsed.sort_unstable();
    allocated.sort_unstable();
    allocations.sort_unstable();
    CandidateMeasurement {
        name: format!("shared_world_candidate_delta_1_retained_{retained}"),
        retained_definitions: retained,
        delta_definitions: 1,
        samples,
        iterations_per_sample: iterations,
        median_ns_per_candidate: elapsed[elapsed.len() / 2] as f64 / iterations as f64,
        min_ns_per_candidate: elapsed[0] as f64 / iterations as f64,
        max_ns_per_candidate: elapsed[elapsed.len() - 1] as f64 / iterations as f64,
        median_allocated_bytes_per_candidate: allocated[allocated.len() / 2] as f64
            / iterations as f64,
        median_allocations_per_candidate: allocations[allocations.len() / 2] as f64
            / iterations as f64,
        checksum,
        notes: "Reaper queue is drained before each sample; candidates remain alive until the timer stops, then background reclamation drains before the next sample.".to_owned(),
    }
}

fn measure_legacy_clone(retained: u64) -> CandidateMeasurement {
    drain_reaper();
    let mut base = LegacyForthDb::new();
    for index in 0..retained {
        base.define(
            SlotId::new(format!("legacy/{index}")),
            literal_fact("legacy", "value", &index.to_string()),
        );
    }
    let (samples, iterations) = dimensions(retained);
    let mut elapsed = Vec::with_capacity(samples);
    let mut allocated = Vec::with_capacity(samples);
    let mut allocations = Vec::with_capacity(samples);
    let mut checksum = 0_u64;

    for sample in 0..samples {
        let before = Counters::capture();
        let started = Instant::now();
        let mut candidates = Vec::with_capacity(iterations as usize);
        for iteration in 0..iterations {
            let mut candidate = black_box(base.clone());
            candidate.define(
                SlotId::new(format!("legacy/new/{sample}/{iteration}")),
                literal_fact("legacy", "value", "new"),
            );
            candidate.validate().expect("legacy candidate validates");
            checksum = checksum.wrapping_add(candidate.record_count() as u64);
            candidates.push(candidate);
        }
        elapsed.push(duration_nanos(started.elapsed()));
        let delta = Counters::capture().since(before);
        allocated.push(delta.allocated_bytes);
        allocations.push(delta.allocations);
        drop(candidates);
    }

    elapsed.sort_unstable();
    allocated.sort_unstable();
    allocations.sort_unstable();
    CandidateMeasurement {
        name: format!("legacy_deep_clone_delta_1_retained_{retained}"),
        retained_definitions: retained,
        delta_definitions: 1,
        samples,
        iterations_per_sample: iterations,
        median_ns_per_candidate: elapsed[elapsed.len() / 2] as f64 / iterations as f64,
        min_ns_per_candidate: elapsed[0] as f64 / iterations as f64,
        max_ns_per_candidate: elapsed[elapsed.len() - 1] as f64 / iterations as f64,
        median_allocated_bytes_per_candidate: allocated[allocated.len() / 2] as f64
            / iterations as f64,
        median_allocations_per_candidate: allocations[allocations.len() / 2] as f64
            / iterations as f64,
        checksum,
        notes: "Control implementation: deep-clones the complete legacy kernel, applies one definition, and performs the legacy full invariant audit.".to_owned(),
    }
}

fn measure_resolve(prepared: &PreparedWorld, retained: u64) -> ReadMeasurement {
    drain_reaper();
    const ITERATIONS: u64 = 1_000_000;
    let mut samples = Vec::with_capacity(5);
    let mut checksum = 0_u64;
    for _ in 0..5 {
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            let fact = black_box(
                prepared
                    .snapshot
                    .resolve(&prepared.hot_slot)
                    .expect("hot slot resolves"),
            );
            checksum = checksum.wrapping_add(fact.predicate.as_str().len() as u64);
        }
        samples.push(duration_nanos(started.elapsed()));
    }
    samples.sort_unstable();
    ReadMeasurement {
        retained_definitions: retained,
        iterations: ITERATIONS,
        median_ns_per_resolve: samples[samples.len() / 2] as f64 / ITERATIONS as f64,
        checksum,
    }
}

fn measure_snapshot_retirement(
    base_definitions: u64,
    snapshot_count: usize,
) -> SnapshotRetirement {
    drain_reaper();
    let database = Database::new(MemoryCommitStore::new()).expect("memory database opens");
    let mut initial = database.begin();
    for index in 0..base_definitions {
        initial.define(
            SlotId::new(format!("pressure/base/{index}")),
            literal_fact("pressure", "base", &index.to_string()),
        );
    }
    database.commit(initial).expect("pressure base commits");

    let mut snapshots = Vec::with_capacity(snapshot_count);
    for index in 0..snapshot_count {
        let mut transaction = database.begin();
        transaction.define(
            SlotId::new(format!("pressure/delta/{index}")),
            literal_fact("pressure", "delta", &index.to_string()),
        );
        database.commit(transaction).expect("pressure delta commits");
        snapshots.push(database.snapshot());
    }
    drain_reaper();
    let metrics_before = ForthDb::reaper_metrics();

    let total_started = Instant::now();
    let mut drops = Vec::with_capacity(snapshot_count);
    for snapshot in snapshots {
        let started = Instant::now();
        drop(snapshot);
        drops.push(duration_nanos(started.elapsed()));
    }
    let total_release_ns = duration_nanos(total_started.elapsed());
    drops.sort_unstable();

    let drop_started = Instant::now();
    drop(database);
    let final_database_drop_ns = duration_nanos(drop_started.elapsed());

    let drain_started = Instant::now();
    let reaper_drained = ForthDb::drain_reaper(Duration::from_secs(60));
    let reaper_drain_ns = duration_nanos(drain_started.elapsed());
    let metrics_after = ForthDb::reaper_metrics();

    SnapshotRetirement {
        base_definitions,
        snapshots: snapshot_count,
        p50_ns: percentile(&drops, 50),
        p95_ns: percentile(&drops, 95),
        p99_ns: percentile(&drops, 99),
        max_ns: *drops.last().unwrap_or(&0),
        total_release_ns,
        final_database_drop_ns,
        reaper_drain_ns,
        reaper_drained,
        metrics_before: metrics_before.into(),
        metrics_after: metrics_after.into(),
        notes: "Foreground drops enqueue shared kernels into the bounded reaper queue. Drain time reports the background cost separately; final database shutdown still releases the world-history spine synchronously.",
    }
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) * percentile) / 100]
}

fn literal_fact(subject: &str, predicate: &str, object: &str) -> Fact {
    Fact::new(
        Atom::Literal(Literal::new(subject)),
        Predicate::new(predicate),
        Atom::Literal(Literal::new(object)),
    )
}

fn duration_nanos(duration: Duration) -> u64 {
    duration.as_nanos().try_into().unwrap_or(u64::MAX)
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}
