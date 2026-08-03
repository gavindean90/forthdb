use forthdb_core::{Atom, EntityId, Fact, ForthDb, Literal, Predicate, SlotId};
use forthdb_world::{
    CommitStore, Database, MemoryCommitStore, QueuedIntent, QueuedIntentController, SubmitError,
    TicketOutcome,
};
use serde::Serialize;
use std::env;
use std::fs;
use std::sync::{mpsc, Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

const RETAINED_DEFINITIONS: usize = 100_000;
const CAPACITY: usize = 256;
const MAX_BATCH: usize = 16;
const BACKPRESSURE_PAUSE: Duration = Duration::from_micros(10);

#[derive(Serialize)]
struct Environment {
    os: &'static str,
    architecture: &'static str,
    profile: &'static str,
}

#[derive(Serialize)]
struct Measurement {
    name: String,
    producers: usize,
    intents: usize,
    abandoned: bool,
    elapsed_ns: u128,
    ns_per_intent: f64,
    intents_per_second: f64,
    epochs: u64,
    average_accepted_per_epoch: f64,
    backpressure_events: u64,
    abandoned_tickets: u64,
    completion_delivery_failures: u64,
    maximum_queue_depth: u64,
}

#[derive(Serialize)]
struct Report {
    status: &'static str,
    retained_definitions: usize,
    capacity: usize,
    max_batch: usize,
    backpressure_pause_ns: u128,
    environment: Environment,
    measurements: Vec<Measurement>,
    total_elapsed_ms: u128,
}

fn base_fact(value: usize) -> Fact {
    Fact::new(
        Atom::Entity(EntityId::new(1)),
        Predicate::new("base_state"),
        Atom::Literal(Literal::new(value.to_string())),
    )
}

fn intent_fact(value: usize) -> Fact {
    Fact::new(
        Atom::Entity(EntityId::new(1)),
        Predicate::new("queued_state"),
        Atom::Literal(Literal::new(value.to_string())),
    )
}

fn build_base_frames() -> Vec<std::sync::Arc<forthdb_world::CommitFrame>> {
    let database = Database::new(MemoryCommitStore::new()).expect("empty store is valid");
    let mut transaction = database.begin();
    assert_eq!(transaction.entity(), EntityId::new(1));
    for index in 0..RETAINED_DEFINITIONS {
        transaction.define(SlotId::new(format!("base/{index}")), base_fact(index));
    }
    database
        .commit(transaction)
        .expect("retained-world setup commits");
    database.frames()
}

fn database_from_frames(
    frames: &[std::sync::Arc<forthdb_world::CommitFrame>],
) -> Arc<Database<MemoryCommitStore>> {
    let mut store = MemoryCommitStore::new();
    for frame in frames {
        match store.append(frame.clone()) {
            Ok(()) => {}
            Err(never) => match never {},
        }
    }
    Arc::new(Database::new(store).expect("base history reconstructs"))
}

fn run_workload(
    base_frames: &[std::sync::Arc<forthdb_world::CommitFrame>],
    name: &str,
    producers: usize,
    intents_per_producer: usize,
    abandon: bool,
) -> Measurement {
    assert!(ForthDb::drain_reaper(Duration::from_secs(30)));
    let database = database_from_frames(base_frames);
    let base_version = database.snapshot().version();
    let controller = Arc::new(
        QueuedIntentController::new(database.clone(), CAPACITY, MAX_BATCH)
            .expect("controller starts"),
    );
    let start_gate = Arc::new(Barrier::new(producers + 1));
    let (ticket_tx, ticket_rx) = mpsc::channel();
    let mut workers = Vec::new();
    let total = producers * intents_per_producer;

    for producer in 0..producers {
        let controller = controller.clone();
        let start_gate = start_gate.clone();
        let ticket_tx = ticket_tx.clone();
        workers.push(thread::spawn(move || {
            start_gate.wait();
            for local in 0..intents_per_producer {
                let sequence = producer * intents_per_producer + local;
                let mut intent = QueuedIntent::new();
                intent.define_fact(
                    SlotId::new(format!("queued/{producer}/{local}")),
                    intent_fact(sequence),
                );
                loop {
                    match controller.submit(intent) {
                        Ok(ticket) => {
                            if abandon {
                                drop(ticket);
                            } else {
                                ticket_tx.send(ticket).expect("collector remains alive");
                            }
                            break;
                        }
                        Err(SubmitError::Full(returned)) => {
                            intent = returned;
                            thread::sleep(BACKPRESSURE_PAUSE);
                        }
                        Err(SubmitError::Closed(_)) => panic!("controller closed"),
                    }
                }
            }
        }));
    }
    drop(ticket_tx);

    let started = Instant::now();
    start_gate.wait();

    // Observe completions while producers are still submitting. This prevents
    // the benchmark from manufacturing a large backlog of resolved Arc<World>
    // values that a normal caller would have consumed promptly.
    let mut observed = 0usize;
    for ticket in ticket_rx {
        match ticket.wait().expect("admitted ticket resolves") {
            TicketOutcome::Accepted { .. } => observed += 1,
            TicketOutcome::Rejected(error) => panic!("benchmark intent rejected: {error}"),
        }
    }
    for worker in workers {
        worker.join().expect("producer does not panic");
    }
    if abandon {
        assert_eq!(observed, 0);
    } else {
        assert_eq!(observed, total);
    }

    controller.flush().expect("controller drains");
    let elapsed = started.elapsed();
    let metrics = controller.metrics();

    assert_eq!(database.snapshot().version(), base_version + total as u64);
    assert_eq!(metrics.submitted, total as u64);
    assert_eq!(metrics.claimed, total as u64);
    assert_eq!(metrics.accepted, total as u64);
    assert_eq!(metrics.rejected, 0);
    assert_eq!(metrics.queue_depth, 0);
    assert_eq!(metrics.in_flight, 0);
    assert!(metrics.maximum_queue_depth <= CAPACITY as u64);
    if abandon {
        assert_eq!(metrics.abandoned_tickets, total as u64);
    }

    let elapsed_ns = elapsed.as_nanos();
    Measurement {
        name: name.to_owned(),
        producers,
        intents: total,
        abandoned: abandon,
        elapsed_ns,
        ns_per_intent: elapsed_ns as f64 / total as f64,
        intents_per_second: total as f64 / elapsed.as_secs_f64(),
        epochs: metrics.epochs,
        average_accepted_per_epoch: total as f64 / metrics.epochs as f64,
        backpressure_events: metrics.backpressured,
        abandoned_tickets: metrics.abandoned_tickets,
        completion_delivery_failures: metrics.completion_delivery_failures,
        maximum_queue_depth: metrics.maximum_queue_depth,
    }
}

fn main() {
    let total_started = Instant::now();
    let base_frames = build_base_frames();
    let measurements = vec![
        run_workload(&base_frames, "single_producer_1024", 1, 1_024, false),
        run_workload(&base_frames, "four_producers_4096", 4, 1_024, false),
        run_workload(&base_frames, "eight_producers_8192", 8, 1_024, false),
        run_workload(
            &base_frames,
            "four_producers_abandoned_4096",
            4,
            1_024,
            true,
        ),
    ];
    assert!(ForthDb::drain_reaper(Duration::from_secs(30)));

    let report = Report {
        status: "observed",
        retained_definitions: RETAINED_DEFINITIONS,
        capacity: CAPACITY,
        max_batch: MAX_BATCH,
        backpressure_pause_ns: BACKPRESSURE_PAUSE.as_nanos(),
        environment: Environment {
            os: env::consts::OS,
            architecture: env::consts::ARCH,
            profile: "release",
        },
        measurements,
        total_elapsed_ms: total_started.elapsed().as_millis(),
    };
    let json = serde_json::to_string_pretty(&report).expect("report serializes");
    if let Ok(path) = env::var("FORTHDB_QUEUED_INGRESS_REPORT") {
        fs::write(path, format!("{json}\n")).expect("report writes");
    } else {
        println!("{json}");
    }
}
