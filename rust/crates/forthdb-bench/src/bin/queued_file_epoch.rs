use forthdb_core::{Atom, EntityId, Fact, ForthDb, Literal, Predicate, SlotId};
use forthdb_world::{
    CommitStore, Database, DurableQueuedIntentController, DurableSubmitError,
    DurableTicketOutcome, FileCommitStore, FileEpochStore, FileEpochSyncPolicy, MemoryCommitStore,
    QueuedIntent,
};
use serde::Serialize;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

const RETAINED_DEFINITIONS: usize = 100_000;
const CAPACITY: usize = 256;
const PRODUCERS: usize = 4;
const BATCH_ONE_INTENTS_PER_PRODUCER: usize = 128;
const BATCH_SIXTEEN_INTENTS_PER_PRODUCER: usize = 512;
const RETRY_PAUSE: Duration = Duration::from_micros(10);

static PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize)]
struct Environment {
    os: &'static str,
    architecture: &'static str,
    profile: &'static str,
}

#[derive(Serialize)]
struct Measurement {
    policy: &'static str,
    max_batch: usize,
    intents: usize,
    producers: usize,
    elapsed_ns: u128,
    ns_per_intent: f64,
    intents_per_second: f64,
    epochs: u64,
    average_batch: f64,
    data_writes: u64,
    data_syncs: u64,
    syncs_per_intent: f64,
    backpressure_events: u64,
    final_version: u64,
    final_frame_count: usize,
    recovered_frame_count: usize,
}

#[derive(Serialize)]
struct Report {
    status: &'static str,
    retained_definitions: usize,
    capacity: usize,
    producers: usize,
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

fn queued_fact(value: usize) -> Fact {
    Fact::new(
        Atom::Entity(EntityId::new(1)),
        Predicate::new("queued_state"),
        Atom::Literal(Literal::new(value.to_string())),
    )
}

fn build_base_frames() -> Vec<Arc<forthdb_world::CommitFrame>> {
    let database = Database::new(MemoryCommitStore::new()).expect("memory store opens");
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

fn temp_path(policy: FileEpochSyncPolicy, max_batch: usize) -> PathBuf {
    let name = match policy {
        FileEpochSyncPolicy::PerFrame => "per-frame",
        FileEpochSyncPolicy::PerEpoch => "per-epoch",
    };
    let sequence = PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "forthdb-m6b-{name}-k{max_batch}-{}-{sequence}.db",
        std::process::id()
    ))
}

fn run_policy(
    base_frames: &[Arc<forthdb_world::CommitFrame>],
    policy: FileEpochSyncPolicy,
    max_batch: usize,
    intents_per_producer: usize,
) -> Measurement {
    assert!(ForthDb::drain_reaper(Duration::from_secs(30)));
    let path = temp_path(policy, max_batch);
    let mut store = FileEpochStore::open(&path, policy).expect("file epoch store opens");
    store
        .append_epoch(base_frames)
        .expect("base frame becomes durable");
    let database = Arc::new(Database::new(store).expect("durable database reconstructs"));
    let base_version = database.snapshot().version();
    let controller = Arc::new(
        DurableQueuedIntentController::new(database.clone(), CAPACITY, max_batch)
            .expect("durable controller starts"),
    );
    let start_gate = Arc::new(Barrier::new(PRODUCERS + 1));
    let (ticket_tx, ticket_rx) = mpsc::channel();
    let mut producers = Vec::new();
    let total = PRODUCERS * intents_per_producer;

    for producer in 0..PRODUCERS {
        let controller = controller.clone();
        let start_gate = start_gate.clone();
        let ticket_tx = ticket_tx.clone();
        producers.push(thread::spawn(move || {
            start_gate.wait();
            for local in 0..intents_per_producer {
                let sequence = producer * intents_per_producer + local;
                let mut intent = QueuedIntent::new();
                intent.define_fact(
                    SlotId::new(format!("queued/{producer}/{local}")),
                    queued_fact(sequence),
                );
                loop {
                    match controller.submit(intent) {
                        Ok(ticket) => {
                            ticket_tx.send(ticket).expect("collector remains alive");
                            break;
                        }
                        Err(DurableSubmitError::Full(returned)) => {
                            intent = returned;
                            thread::sleep(RETRY_PAUSE);
                        }
                        Err(DurableSubmitError::Closed(_)) => {
                            panic!("durable controller closed")
                        }
                    }
                }
            }
        }));
    }
    drop(ticket_tx);

    let started = Instant::now();
    start_gate.wait();
    for _ in 0..total {
        let ticket = ticket_rx.recv().expect("every admitted ticket is collected");
        match ticket.wait().expect("durable ticket resolves") {
            DurableTicketOutcome::Accepted { .. } => {}
            DurableTicketOutcome::Rejected(error) => panic!("intent rejected: {error}"),
            DurableTicketOutcome::DurabilityFailed(error) => {
                panic!("durability failed during benchmark: {error}")
            }
        }
    }
    for producer in producers {
        producer.join().expect("producer does not panic");
    }
    controller.flush().expect("durable controller drains");
    let elapsed = started.elapsed();
    let controller_metrics = controller.metrics();
    let store_metrics = controller.store_metrics();
    let final_version = database.snapshot().version();
    let final_frame_count = database.frame_count();

    assert_eq!(controller_metrics.submitted, total as u64);
    assert_eq!(controller_metrics.claimed, total as u64);
    assert_eq!(controller_metrics.accepted, total as u64);
    assert_eq!(controller_metrics.rejected, 0);
    assert_eq!(controller_metrics.durability_failed, 0);
    assert_eq!(final_version, base_version + total as u64);
    assert_eq!(final_frame_count, base_frames.len() + total);

    drop(controller);
    drop(database);
    let recovered = FileCommitStore::open(&path).expect("durable file reopens");
    let recovered_frame_count = recovered.len();
    assert_eq!(recovered_frame_count, final_frame_count);
    drop(recovered);
    let _ = fs::remove_file(&path);
    assert!(ForthDb::drain_reaper(Duration::from_secs(30)));

    let elapsed_ns = elapsed.as_nanos();
    let data_syncs = store_metrics.data_syncs.saturating_sub(1);
    Measurement {
        policy: match policy {
            FileEpochSyncPolicy::PerFrame => "per_frame",
            FileEpochSyncPolicy::PerEpoch => "per_epoch",
        },
        max_batch,
        intents: total,
        producers: PRODUCERS,
        elapsed_ns,
        ns_per_intent: elapsed_ns as f64 / total as f64,
        intents_per_second: total as f64 / elapsed.as_secs_f64(),
        epochs: controller_metrics.epochs,
        average_batch: total as f64 / controller_metrics.epochs as f64,
        data_writes: store_metrics.data_writes,
        data_syncs,
        syncs_per_intent: data_syncs as f64 / total as f64,
        backpressure_events: controller_metrics.backpressured,
        final_version,
        final_frame_count,
        recovered_frame_count,
    }
}

fn main() {
    let total_started = Instant::now();
    let base_frames = build_base_frames();
    let measurements = vec![
        run_policy(
            &base_frames,
            FileEpochSyncPolicy::PerFrame,
            1,
            BATCH_ONE_INTENTS_PER_PRODUCER,
        ),
        run_policy(
            &base_frames,
            FileEpochSyncPolicy::PerEpoch,
            1,
            BATCH_ONE_INTENTS_PER_PRODUCER,
        ),
        run_policy(
            &base_frames,
            FileEpochSyncPolicy::PerFrame,
            16,
            BATCH_SIXTEEN_INTENTS_PER_PRODUCER,
        ),
        run_policy(
            &base_frames,
            FileEpochSyncPolicy::PerEpoch,
            16,
            BATCH_SIXTEEN_INTENTS_PER_PRODUCER,
        ),
    ];
    let report = Report {
        status: "observed",
        retained_definitions: RETAINED_DEFINITIONS,
        capacity: CAPACITY,
        producers: PRODUCERS,
        environment: Environment {
            os: env::consts::OS,
            architecture: env::consts::ARCH,
            profile: "release",
        },
        measurements,
        total_elapsed_ms: total_started.elapsed().as_millis(),
    };
    let json = serde_json::to_string_pretty(&report).expect("report serializes");
    if let Ok(path) = env::var("FORTHDB_FILE_EPOCH_REPORT") {
        fs::write(path, format!("{json}\n")).expect("report writes");
    } else {
        println!("{json}");
    }
}
