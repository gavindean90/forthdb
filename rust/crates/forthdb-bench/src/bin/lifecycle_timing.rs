use forthdb_core::{Atom, EntityId, Fact, ForthDb, Literal, Predicate, SlotId};
use forthdb_world::{
    Database, DurableQueuedIntentController, DurableSubmitError, DurableTicketOutcome,
    FileEpochStore, FileEpochSyncPolicy, MemoryCommitStore, QueuedIntent,
};
use serde::Serialize;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_RETAINED_DEFINITIONS: usize = 100_000;
const DEFAULT_INTENTS: usize = 2_048;
const CAPACITY: usize = 256;
const MAX_BATCH: usize = 16;
const PRODUCERS: usize = 4;
const RETRY_PAUSE: Duration = Duration::from_micros(10);

static PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize)]
struct Report {
    status: &'static str,
    retained_definitions: usize,
    intents: usize,
    producers: usize,
    capacity: usize,
    max_batch: usize,
    elapsed_nanos: u128,
    intents_per_second: f64,
    epochs: u64,
    average_batch: f64,
    backpressure_events: u64,
    queue_wait_nanos: u64,
    derive_nanos: u64,
    persist_nanos: u64,
    publish_nanos: u64,
    delivery_nanos: u64,
    epoch_total_nanos: u64,
    derive_nanos_per_epoch: f64,
    persist_nanos_per_epoch: f64,
    epoch_total_nanos_per_epoch: f64,
    core_stage_speedup_ceiling: f64,
    full_pipeline_speedup_ceiling: f64,
    data_writes: u64,
    data_syncs: u64,
    final_version: u64,
    final_frame_count: usize,
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn temp_path() -> PathBuf {
    let sequence = PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!(
        "forthdb-m6d-lifecycle-timing-{}-{sequence}.db",
        std::process::id()
    ))
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

fn build_base_frames(retained_definitions: usize) -> Vec<Arc<forthdb_world::CommitFrame>> {
    let database = Database::new(MemoryCommitStore::new()).expect("memory store opens");
    let mut transaction = database.begin();
    assert_eq!(transaction.entity(), EntityId::new(1));
    for index in 0..retained_definitions {
        transaction.define(SlotId::new(format!("base/{index}")), base_fact(index));
    }
    database.commit(transaction).expect("base world commits");
    database.frames()
}

fn main() {
    let retained_definitions = env_usize(
        "FORTHDB_M6D_RETAINED_DEFINITIONS",
        DEFAULT_RETAINED_DEFINITIONS,
    );
    let intents = env_usize("FORTHDB_M6D_INTENTS", DEFAULT_INTENTS);
    assert!(intents >= PRODUCERS);
    assert_eq!(intents % PRODUCERS, 0);
    assert!(ForthDb::drain_reaper(Duration::from_secs(30)));

    let base_frames = build_base_frames(retained_definitions);
    let path = temp_path();
    let mut store =
        FileEpochStore::open(&path, FileEpochSyncPolicy::PerEpoch).expect("file epoch store opens");
    store
        .append_epoch(&base_frames)
        .expect("base frame becomes durable");
    let database = Arc::new(Database::new(store).expect("durable database reconstructs"));
    let base_version = database.snapshot().version();
    let controller = Arc::new(
        DurableQueuedIntentController::new(database.clone(), CAPACITY, MAX_BATCH)
            .expect("durable controller starts"),
    );

    let intents_per_producer = intents / PRODUCERS;
    let start_gate = Arc::new(Barrier::new(PRODUCERS + 1));
    let (ticket_tx, ticket_rx) = mpsc::channel();
    let mut producers = Vec::with_capacity(PRODUCERS);
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
                            panic!("controller closed during observation")
                        }
                        Err(DurableSubmitError::Poisoned { reason, .. }) => {
                            panic!("controller poisoned during observation: {reason}")
                        }
                    }
                }
            }
        }));
    }
    drop(ticket_tx);

    let started = Instant::now();
    start_gate.wait();
    for _ in 0..intents {
        let ticket = ticket_rx
            .recv()
            .expect("every admitted ticket is collected");
        match ticket.wait().expect("ticket resolves") {
            DurableTicketOutcome::Accepted { .. } => {}
            DurableTicketOutcome::Rejected(error) => panic!("intent rejected: {error}"),
            DurableTicketOutcome::DurabilityFailed(error) => {
                panic!("durability failed: {error}")
            }
            DurableTicketOutcome::Stopped(reason) => {
                panic!("controller stopped during observation: {reason:?}")
            }
        }
    }
    for producer in producers {
        producer.join().expect("producer does not panic");
    }
    controller
        .flush()
        .expect("controller reaches timing barrier");
    let elapsed = started.elapsed();

    let metrics = controller.metrics();
    let store_metrics = controller.store_metrics();
    assert_eq!(metrics.accepted, intents as u64);
    assert_eq!(metrics.rejected, 0);
    assert_eq!(metrics.durability_failed, 0);
    assert_eq!(metrics.shutdown_before_claim, 0);
    assert_eq!(metrics.worker_failed, 0);
    assert_eq!(database.snapshot().version(), base_version + intents as u64);

    let core_stage_speedup_ceiling = metrics
        .estimated_pipeline_speedup_ceiling()
        .expect("derive and persist timing are populated");
    let overlap_savings = metrics.derive_nanos.min(metrics.persist_nanos);
    let ideal_full_pipeline_nanos = metrics.epoch_total_nanos.saturating_sub(overlap_savings);
    let full_pipeline_speedup_ceiling =
        metrics.epoch_total_nanos as f64 / ideal_full_pipeline_nanos as f64;
    let epochs = metrics.epochs.max(1);

    let report = Report {
        status: "observed",
        retained_definitions,
        intents,
        producers: PRODUCERS,
        capacity: CAPACITY,
        max_batch: MAX_BATCH,
        elapsed_nanos: elapsed.as_nanos(),
        intents_per_second: intents as f64 / elapsed.as_secs_f64(),
        epochs: metrics.epochs,
        average_batch: intents as f64 / metrics.epochs as f64,
        backpressure_events: metrics.backpressured,
        queue_wait_nanos: metrics.queue_wait_nanos,
        derive_nanos: metrics.derive_nanos,
        persist_nanos: metrics.persist_nanos,
        publish_nanos: metrics.publish_nanos,
        delivery_nanos: metrics.delivery_nanos,
        epoch_total_nanos: metrics.epoch_total_nanos,
        derive_nanos_per_epoch: metrics.derive_nanos as f64 / epochs as f64,
        persist_nanos_per_epoch: metrics.persist_nanos as f64 / epochs as f64,
        epoch_total_nanos_per_epoch: metrics.epoch_total_nanos as f64 / epochs as f64,
        core_stage_speedup_ceiling,
        full_pipeline_speedup_ceiling,
        data_writes: store_metrics.data_writes.saturating_sub(1),
        data_syncs: store_metrics.data_syncs.saturating_sub(1),
        final_version: database.snapshot().version(),
        final_frame_count: database.frame_count(),
    };

    let shutdown = controller.shutdown();
    assert_eq!(
        shutdown.final_state,
        forthdb_world::DurableControllerState::Closed
    );
    drop(controller);
    drop(database);
    let _ = fs::remove_file(path);
    assert!(ForthDb::drain_reaper(Duration::from_secs(30)));

    let json = serde_json::to_string_pretty(&report).expect("report serializes");
    if let Ok(path) = env::var("FORTHDB_M6D_TIMING_REPORT") {
        fs::write(path, format!("{json}\n")).expect("report writes");
    } else {
        println!("{json}");
    }
}
