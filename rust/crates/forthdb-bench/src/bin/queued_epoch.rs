use forthdb_core::{Atom, Fact, ForthDb, Literal, Predicate, SlotId};
use forthdb_world::{
    derive_epoch, Database, IntentFact, MemoryCommitStore, QueuedIntent, World,
};
use serde::Serialize;
use std::env;
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WARMUP_EPOCHS: usize = 3;
const SAMPLES: usize = 21;
const DEFAULT_RETAINED_DEFINITIONS: u64 = 100_000;

#[derive(Serialize)]
struct Report {
    implementation: &'static str,
    scope: &'static str,
    status: &'static str,
    profile: &'static str,
    retained_definitions: u64,
    measurements: Vec<Measurement>,
    total_elapsed_ms: u64,
    environment: Environment,
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
struct Measurement {
    batch_size: usize,
    warmup_epochs: usize,
    epochs_measured: usize,
    median_ns_per_epoch: f64,
    median_ns_per_intent: f64,
    p95_ns_per_intent: f64,
    min_ns_per_intent: f64,
    max_ns_per_intent: f64,
    median_intents_per_second: f64,
    epoch_elapsed_ns: Vec<u64>,
    checksum: u64,
    notes: String,
}

fn main() {
    let started = Instant::now();
    let retained_definitions = env::var("FORTHDB_M6_RETAINED_DEFINITIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_RETAINED_DEFINITIONS);
    let base = build_base(retained_definitions);
    let measurements = [1, 2, 4, 8, 16, 32, 64]
        .into_iter()
        .map(|batch_size| measure_batch(base.clone(), batch_size))
        .collect();

    let report = Report {
        implementation: "rust",
        scope: "milestone-6a-private-queued-epoch-derivation",
        status: "observational",
        profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        retained_definitions,
        measurements,
        total_elapsed_ms: millis(started.elapsed()),
        environment: Environment {
            os: env::consts::OS,
            architecture: env::consts::ARCH,
            crate_version: env!("CARGO_PKG_VERSION"),
            git_sha: env::var("GITHUB_SHA").ok(),
            github_run_id: env::var("GITHUB_RUN_ID").ok(),
        },
    };

    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("queued epoch report serializes")
    );
}

fn build_base(retained_definitions: u64) -> Arc<World> {
    let database = Database::new(MemoryCommitStore::new()).expect("empty store valid");
    let mut transaction = database.begin();
    for index in 0..retained_definitions {
        transaction.define(
            SlotId::new(format!("retained/{index}")),
            Fact::new(
                Atom::Literal(Literal::new("retained")),
                Predicate::new("value"),
                Atom::Literal(Literal::new(index.to_string())),
            ),
        );
    }
    database
        .commit(transaction)
        .expect("retained base commits");
    database.snapshot()
}

fn measure_batch(base: Arc<World>, batch_size: usize) -> Measurement {
    let mut checksum = 0_u64;

    for warmup in 0..WARMUP_EPOCHS {
        assert!(ForthDb::drain_reaper(Duration::from_secs(30)));
        let plan = black_box(derive_epoch(
            base.clone(),
            build_intents(usize::MAX - warmup, batch_size),
            &[],
        ));
        checksum = checksum.wrapping_add(plan.tail().id().value());
        drop(plan);
        assert!(ForthDb::drain_reaper(Duration::from_secs(30)));
    }

    let mut elapsed = Vec::with_capacity(SAMPLES);
    for sample in 0..SAMPLES {
        assert!(
            ForthDb::drain_reaper(Duration::from_secs(30)),
            "semantic-kernel reaper must be empty before a sample"
        );
        let intents = build_intents(sample, batch_size);

        let began = Instant::now();
        let plan = black_box(derive_epoch(base.clone(), intents, &[]));
        let duration = nanos(began.elapsed());
        checksum = checksum
            .wrapping_add(plan.tail().id().value())
            .wrapping_add(plan.accepted_count() as u64);
        black_box(plan.outcomes());
        elapsed.push(duration);

        drop(plan);
        assert!(
            ForthDb::drain_reaper(Duration::from_secs(30)),
            "semantic-kernel reaper must drain after a sample"
        );
    }

    elapsed.sort_unstable();
    let ns_per_intent: Vec<f64> = elapsed
        .iter()
        .map(|value| *value as f64 / batch_size as f64)
        .collect();
    let median_index = SAMPLES / 2;
    let p95_index = ((SAMPLES as f64 * 0.95).ceil() as usize - 1).min(SAMPLES - 1);
    let median_per_intent = ns_per_intent[median_index];

    Measurement {
        batch_size,
        warmup_epochs: WARMUP_EPOCHS,
        epochs_measured: SAMPLES,
        median_ns_per_epoch: elapsed[median_index] as f64,
        median_ns_per_intent: median_per_intent,
        p95_ns_per_intent: ns_per_intent[p95_index],
        min_ns_per_intent: ns_per_intent[0],
        max_ns_per_intent: ns_per_intent[SAMPLES - 1],
        median_intents_per_second: 1_000_000_000.0 / median_per_intent,
        epoch_elapsed_ns: elapsed,
        checksum,
        notes: "Each observation times one complete epoch after warm-up. Intent construction is complete before timing; every intermediate world remains live until timing stops; the semantic-kernel reaper is drained before and after every epoch.".to_owned(),
    }
}

fn build_intents(sample: usize, batch_size: usize) -> Vec<QueuedIntent> {
    (0..batch_size)
        .map(|position| {
            let mut intent = QueuedIntent::new();
            let entity = intent.entity();
            intent.define(
                SlotId::new(format!("epoch/{sample}/{position}")),
                IntentFact::new(
                    entity,
                    Predicate::new("state"),
                    Literal::new("ready"),
                ),
            );
            intent
        })
        .collect()
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
