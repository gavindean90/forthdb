use forthdb_core::{Atom, Fact, ForthDb, IntentAtomPlaceholder, Literal, Predicate, SlotId};
use forthdb_world::{
    derive_epoch, Database, IntentFact, MemoryCommitStore, QueuedIntent, World,
};
use serde::Serialize;
use std::env;
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SAMPLES: usize = 3;
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
    iterations_per_sample: usize,
    samples: usize,
    median_ns_per_epoch: f64,
    median_ns_per_intent: f64,
    min_ns_per_intent: f64,
    max_ns_per_intent: f64,
    median_intents_per_second: f64,
    sample_elapsed_ns: Vec<u64>,
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
    let iterations = match batch_size {
        1..=4 => 200,
        5..=16 => 100,
        17..=32 => 50,
        _ => 25,
    };
    let mut elapsed = Vec::with_capacity(SAMPLES);
    let mut checksum = 0_u64;

    for sample in 0..SAMPLES {
        ForthDb::drain_reaper(Duration::from_secs(30));
        let batches: Vec<Vec<QueuedIntent>> = (0..iterations)
            .map(|iteration| build_intents(sample, iteration, batch_size))
            .collect();
        let mut retained_plans = Vec::with_capacity(iterations);

        let began = Instant::now();
        for intents in batches {
            let plan = black_box(derive_epoch(base.clone(), intents, &[]));
            checksum = checksum
                .wrapping_add(plan.tail().id().value())
                .wrapping_add(plan.accepted_count() as u64);
            retained_plans.push(plan);
        }
        elapsed.push(nanos(began.elapsed()));

        drop(retained_plans);
        assert!(
            ForthDb::drain_reaper(Duration::from_secs(30)),
            "semantic-kernel reaper must drain between samples"
        );
    }

    elapsed.sort_unstable();
    let intents_per_sample = iterations * batch_size;
    let ns_per_intent: Vec<f64> = elapsed
        .iter()
        .map(|value| *value as f64 / intents_per_sample as f64)
        .collect();
    let median_per_intent = ns_per_intent[SAMPLES / 2];
    let median_per_epoch = elapsed[SAMPLES / 2] as f64 / iterations as f64;

    Measurement {
        batch_size,
        iterations_per_sample: iterations,
        samples: SAMPLES,
        median_ns_per_epoch: median_per_epoch,
        median_ns_per_intent: median_per_intent,
        min_ns_per_intent: ns_per_intent[0],
        max_ns_per_intent: ns_per_intent[SAMPLES - 1],
        median_intents_per_second: 1_000_000_000.0 / median_per_intent,
        sample_elapsed_ns: elapsed,
        checksum,
        notes: "Intent construction occurs before timing; candidate worlds remain live until the timer stops; background reaping is drained before and after each sample.".to_owned(),
    }
}

fn build_intents(sample: usize, iteration: usize, batch_size: usize) -> Vec<QueuedIntent> {
    (0..batch_size)
        .map(|position| {
            let mut intent = QueuedIntent::new();
            let entity = intent.entity();
            intent.define(
                SlotId::new(format!("epoch/{sample}/{iteration}/{position}")),
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
