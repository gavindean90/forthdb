use forthdb_core::{
    Atom, ForthDb, Literal, Pattern, Predicate, PredicateTerm, QueryOptions, SlotId, Term, Variable,
};
use serde::Serialize;
use std::env;
use std::hint::black_box;
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

fn main() {
    let started = Instant::now();
    let measurements = vec![
        measure_with_setup(
            "define_unique_slots",
            "definition",
            50_000,
            || (),
            |_| define_unique_slots(50_000),
            "Includes immutable record append and all current-head index updates.",
        ),
        measure_with_setup(
            "redefine_one_slot",
            "definition",
            50_000,
            || (),
            |_| redefine_one_slot(50_000),
            "Replaces one active head while retaining all prior definitions.",
        ),
        resolve_measurement(1, 500_000),
        resolve_measurement(1_000, 500_000),
        resolve_measurement(50_000, 500_000),
        exact_query_measurement(20_000, 20_000),
        indexed_variable_query_measurement(20_000, 20_000),
        two_hop_join_measurement(64, 2_000),
        forget_measurement(30_000, 10_000),
    ];

    let report = BenchmarkReport {
        implementation: "rust",
        scope: "in-memory-semantic-kernel",
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

fn define_unique_slots(count: u64) -> u64 {
    let mut db = ForthDb::new();
    let subject = Atom::Literal(Literal::new("benchmark"));
    let predicate = Predicate::new("value");
    for index in 0..count {
        db.define(
            SlotId::new(format!("benchmark/value/{index}")),
            forthdb_core::Fact::new(
                subject.clone(),
                predicate.clone(),
                Atom::Literal(Literal::new(index.to_string())),
            ),
        );
    }
    black_box((db.record_count() + db.active_slot_count()) as u64)
}

fn redefine_one_slot(count: u64) -> u64 {
    let mut db = ForthDb::new();
    let slot = SlotId::new("benchmark/current");
    let subject = Atom::Literal(Literal::new("benchmark"));
    let predicate = Predicate::new("current");
    for index in 0..count {
        db.define(
            slot.clone(),
            forthdb_core::Fact::new(
                subject.clone(),
                predicate.clone(),
                Atom::Literal(Literal::new(index.to_string())),
            ),
        );
    }
    black_box(db.record_count() as u64)
}

fn resolve_measurement(history_depth: u64, iterations: u64) -> Measurement {
    let name = match history_depth {
        1 => "resolve_current_head_history_1",
        1_000 => "resolve_current_head_history_1000",
        50_000 => "resolve_current_head_history_50000",
        _ => "resolve_current_head",
    };
    measure_with_setup(
        name,
        "resolution",
        iterations,
        move || build_deep_history(history_depth),
        move |(db, slot)| {
            let mut checksum = 0_u64;
            for _ in 0..iterations {
                let fact = black_box(db.resolve(slot).expect("benchmark slot must resolve"));
                checksum = checksum.wrapping_add(match &fact.object {
                    Atom::Literal(value) => value.as_str().len() as u64,
                    Atom::Entity(entity) => entity.value(),
                });
            }
            checksum
        },
        "Current resolution follows the active slot head; retained history is not scanned.",
    )
}

fn build_deep_history(depth: u64) -> (ForthDb, SlotId) {
    let mut db = ForthDb::new();
    let slot = SlotId::new("benchmark/deep");
    let subject = Atom::Literal(Literal::new("deep"));
    let predicate = Predicate::new("state");
    for index in 0..depth {
        db.define(
            slot.clone(),
            forthdb_core::Fact::new(
                subject.clone(),
                predicate.clone(),
                Atom::Literal(Literal::new(index.to_string())),
            ),
        );
    }
    (db, slot)
}

fn exact_query_measurement(dataset_size: u64, iterations: u64) -> Measurement {
    measure_with_setup(
        "exact_fact_query",
        "query",
        iterations,
        move || {
            let mut db = ForthDb::new();
            let predicate = Predicate::new("state");
            let object = Atom::Literal(Literal::new("ready"));
            let mut target = None;
            for index in 0..dataset_size {
                let entity = db.entity();
                if index == dataset_size / 2 {
                    target = Some(entity);
                }
                db.define(
                    SlotId::new(format!("entity/{index}/state")),
                    forthdb_core::Fact::new(
                        Atom::Entity(entity),
                        predicate.clone(),
                        object.clone(),
                    ),
                );
            }
            let pattern = Pattern::new(
                Term::Atom(Atom::Entity(target.expect("dataset has a midpoint"))),
                PredicateTerm::Predicate(predicate),
                Term::Atom(object),
            );
            (db, vec![pattern])
        },
        move |(db, patterns)| {
            let mut checksum = 0_u64;
            for _ in 0..iterations {
                checksum = checksum.wrapping_add(black_box(
                    db.query(patterns, QueryOptions::default()).rows.len(),
                ) as u64);
            }
            checksum
        },
        "Exact subject-predicate-object lookup in a 20,000-fact active index.",
    )
}

fn indexed_variable_query_measurement(dataset_size: u64, iterations: u64) -> Measurement {
    measure_with_setup(
        "indexed_subject_predicate_query",
        "query",
        iterations,
        move || {
            let mut db = ForthDb::new();
            let predicate = Predicate::new("state");
            let mut target = None;
            for index in 0..dataset_size {
                let entity = db.entity();
                if index == dataset_size / 2 {
                    target = Some(entity);
                }
                db.define(
                    SlotId::new(format!("entity/{index}/state")),
                    forthdb_core::Fact::new(
                        Atom::Entity(entity),
                        predicate.clone(),
                        Atom::Literal(Literal::new(index.to_string())),
                    ),
                );
            }
            let pattern = Pattern::new(
                Term::Atom(Atom::Entity(target.expect("dataset has a midpoint"))),
                PredicateTerm::Predicate(predicate),
                Term::Variable(Variable::new("value").expect("valid benchmark variable")),
            );
            (db, vec![pattern])
        },
        move |(db, patterns)| {
            let mut checksum = 0_u64;
            for _ in 0..iterations {
                checksum = checksum.wrapping_add(black_box(
                    db.query(patterns, QueryOptions::default()).rows.len(),
                ) as u64);
            }
            checksum
        },
        "Subject-predicate index lookup with one object binding in a 20,000-fact dataset.",
    )
}

fn two_hop_join_measurement(fanout: u64, iterations: u64) -> Measurement {
    measure_with_setup(
        "two_hop_join_fanout_64",
        "query",
        iterations,
        move || {
            let mut db = ForthDb::new();
            let work = db.entity();
            for index in 0..fanout {
                let copy = db.entity();
                let shelf = db.entity();
                db.define(
                    SlotId::new(format!("work/copy/{index}")),
                    forthdb_core::Fact::new(
                        Atom::Entity(work),
                        Predicate::new("has_copy"),
                        Atom::Entity(copy),
                    ),
                );
                db.define(
                    SlotId::new(format!("copy/{index}/location")),
                    forthdb_core::Fact::new(
                        Atom::Entity(copy),
                        Predicate::new("located_at"),
                        Atom::Entity(shelf),
                    ),
                );
            }
            let copy_variable = Variable::new("copy").expect("valid benchmark variable");
            let patterns = vec![
                Pattern::new(
                    Term::Atom(Atom::Entity(work)),
                    PredicateTerm::Predicate(Predicate::new("has_copy")),
                    Term::Variable(copy_variable.clone()),
                ),
                Pattern::new(
                    Term::Variable(copy_variable),
                    PredicateTerm::Predicate(Predicate::new("located_at")),
                    Term::Variable(Variable::new("shelf").expect("valid benchmark variable")),
                ),
            ];
            (db, patterns)
        },
        move |(db, patterns)| {
            let mut checksum = 0_u64;
            for _ in 0..iterations {
                checksum = checksum.wrapping_add(black_box(
                    db.query(patterns, QueryOptions::default()).rows.len(),
                ) as u64);
            }
            checksum
        },
        "Returns 64 joined rows from two indexed patterns per query.",
    )
}

fn forget_measurement(history_depth: u64, forget_count: u64) -> Measurement {
    measure_with_setup(
        "forget_reveals_previous_head",
        "forget",
        forget_count,
        move || build_deep_history(history_depth),
        move |(db, slot)| {
            let mut checksum = 0_u64;
            for _ in 0..forget_count {
                checksum = checksum.wrapping_add(db.forget(slot.clone()).value() as u64);
            }
            checksum.wrapping_add(db.active_slot_count() as u64)
        },
        "Each forget appends a record and restores the immediately previous definition head.",
    )
}

fn duration_nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}
