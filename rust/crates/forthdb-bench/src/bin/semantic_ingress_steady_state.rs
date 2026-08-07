use std::sync::Arc;
use std::time::{Duration, Instant};

use forthdb_core::{Literal, Predicate, SlotId};
use forthdb_world::transaction_ast::{AtomRef, SemanticIntent, TransactionAST, TransactionOp};
use forthdb_world::{
    BatchPolicy, ControllerIntent, Database, IntentAtom, IntentFact, MemoryCommitStore, QueuedIntent,
    QueuedIntentController, SemanticTicketOutcome, VmEpochMaterializer,
};

const MEASURED_INTENTS: usize = 5_000;

fn populate_base_store(size: usize) -> MemoryCommitStore {
    let store = MemoryCommitStore::new();
    if size == 0 {
        return store;
    }

    let database = Arc::new(Database::new(store).unwrap());
    let mut materializer = VmEpochMaterializer::new(1);
    let batch_size = 10_000;

    for batch_start in (0..size).step_by(batch_size) {
        let chunk_end = (batch_start + batch_size).min(size);
        let mut intents = Vec::with_capacity(chunk_end - batch_start);

        for i in batch_start..chunk_end {
            let mut queued = QueuedIntent::new();
            let entity = queued.entity();
            queued.define(
                SlotId::new(&format!("init_slot_{i}")),
                IntentFact {
                    subject: IntentAtom::Temporary(entity),
                    predicate: Predicate::new("init_pred"),
                    object: IntentAtom::Literal(Literal::new(&format!("init_val_{i}"))),
                },
            );
            intents.push(ControllerIntent::Queued(queued));
        }

        database.commit_mixed_epoch(intents, &mut materializer);
    }

    database.store_clone()
}

fn synchronize_controller(controller: &QueuedIntentController) -> Duration {
    let start = Instant::now();
    let ticket = controller
        .submit_semantic(SemanticIntent::new(TransactionAST::new(
            u64::MAX - 1,
            vec![TransactionOp::Reject],
        )))
        .expect("synchronization probe must submit");

    match ticket.wait().expect("synchronization probe must resolve") {
        SemanticTicketOutcome::Rejected(_) => {}
        SemanticTicketOutcome::Accepted { .. } => {
            panic!("synchronization probe must reject without publishing state")
        }
    }

    controller.flush().expect("synchronization flush must complete");
    start.elapsed()
}

fn warm_vocabulary(controller: &QueuedIntentController) {
    let ticket = controller
        .submit_semantic(SemanticIntent::new(TransactionAST::new(
            u64::MAX - 2,
            vec![TransactionOp::Define {
                slot: "bench_warm_slot".to_owned(),
                subject: AtomRef::Literal("bench_warm_subject".to_owned()),
                predicate: "bench_warm_pred".to_owned(),
                object: AtomRef::Literal("bench_warm_value".to_owned()),
            }],
        )))
        .expect("vocabulary warm-up must submit");

    match ticket.wait().expect("vocabulary warm-up must resolve") {
        SemanticTicketOutcome::Accepted { .. } => {}
        SemanticTicketOutcome::Rejected(error) => {
            panic!("vocabulary warm-up unexpectedly rejected: {error}")
        }
    }

    controller.flush().expect("vocabulary warm-up flush must complete");
}

fn run_state_case(label: &str, retained_definitions: usize) {
    let base_store = populate_base_store(retained_definitions);
    let database = Arc::new(Database::new(base_store).unwrap());
    let controller = QueuedIntentController::new(
        database.clone(),
        100_000,
        BatchPolicy::Adaptive {
            min_batch: 1,
            max_batch: 64,
            latency_budget: Duration::from_millis(1),
        },
    )
    .unwrap();

    // Force retained-state replay/synchronization before the steady-state timing window.
    let cold_sync = synchronize_controller(&controller);

    // Make the measured vocabulary genuinely warm, and wait for completion instead of sleeping.
    warm_vocabulary(&controller);
    let timed_base = database.snapshot();

    let start = Instant::now();
    let mut tickets = Vec::with_capacity(MEASURED_INTENTS);

    for i in 0..MEASURED_INTENTS {
        let intent = SemanticIntent::new(TransactionAST::new(
            i as u64,
            vec![TransactionOp::Define {
                slot: "bench_warm_slot".to_owned(),
                subject: AtomRef::Literal("bench_warm_subject".to_owned()),
                predicate: "bench_warm_pred".to_owned(),
                object: AtomRef::Literal("bench_warm_value".to_owned()),
            }],
        ));
        tickets.push(
            controller
                .submit_semantic(intent)
                .expect("steady-state semantic intent must submit"),
        );
    }

    let mut accepted = 0usize;
    for ticket in tickets {
        match ticket.wait().expect("steady-state ticket must resolve") {
            SemanticTicketOutcome::Accepted { world, bindings } => {
                accepted += 1;
                std::hint::black_box((world, bindings));
            }
            SemanticTicketOutcome::Rejected(error) => {
                panic!("steady-state warm intent unexpectedly rejected: {error}")
            }
        }
    }

    controller.flush().expect("steady-state flush must complete");
    let elapsed = start.elapsed();
    let final_world = database.snapshot();

    assert_eq!(accepted, MEASURED_INTENTS);
    assert!(final_world.version() > timed_base.version());

    let tps = MEASURED_INTENTS as f64 / elapsed.as_secs_f64();
    println!(
        "{label:<18} | cold sync {:>9.3} ms | steady warm {:>10.1} TPS | {:>8.1} ns/op",
        cold_sync.as_secs_f64() * 1_000.0,
        tps,
        elapsed.as_nanos() as f64 / MEASURED_INTENTS as f64,
    );
}

fn main() {
    println!("ForthDB semantic ingress: cold synchronization vs steady-state warm vocabulary");
    println!("Adaptive policy, one producer, {MEASURED_INTENTS} measured intents per state tier");
    println!();

    run_state_case("Genesis / Tiny", 0);
    run_state_case("10,000 Defs", 10_000);
    run_state_case("100,000 Defs", 100_000);
}
