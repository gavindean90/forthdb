use forthdb_core::{Atom, EntityId, Fact, Literal, Predicate, SlotId};
use forthdb_world::{
    Database, MemoryCommitStore, QueuedIntent, QueuedIntentController, SubmitError, TicketOutcome,
    TicketPhase,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::thread;
use std::time::Duration;

const PRODUCERS: usize = 8;
const INTENTS_PER_PRODUCER: usize = 250;

fn state_fact(entity: EntityId, value: String) -> Fact {
    Fact::new(
        Atom::Entity(entity),
        Predicate::new("state"),
        Atom::Literal(Literal::new(value)),
    )
}

#[test]
fn concurrent_producers_lose_or_duplicate_no_admitted_intents() {
    let database =
        Arc::new(Database::new(MemoryCommitStore::new()).expect("empty memory store is valid"));
    let controller =
        Arc::new(QueuedIntentController::new(database.clone(), 64, 16).expect("controller starts"));
    let start = Arc::new(Barrier::new(PRODUCERS + 1));
    let (ticket_tx, ticket_rx) = mpsc::channel();
    let mut workers = Vec::new();

    for producer in 0..PRODUCERS {
        let controller = controller.clone();
        let start = start.clone();
        let ticket_tx = ticket_tx.clone();
        workers.push(thread::spawn(move || {
            start.wait();
            for local in 0..INTENTS_PER_PRODUCER {
                let sequence = producer * INTENTS_PER_PRODUCER + local;
                let mut intent = QueuedIntent::new();
                intent.define_fact(
                    SlotId::new(format!("concurrent/{producer}/{local}")),
                    state_fact(EntityId::new(1), sequence.to_string()),
                );

                loop {
                    match controller.submit(intent) {
                        Ok(ticket) => {
                            ticket_tx.send(ticket).expect("collector remains alive");
                            break;
                        }
                        Err(SubmitError::Full(returned)) => {
                            intent = returned;
                            thread::yield_now();
                        }
                        Err(SubmitError::Closed(_)) => panic!("controller closed during stress"),
                    }
                }
            }
        }));
    }
    drop(ticket_tx);
    start.wait();

    for worker in workers {
        worker.join().expect("producer does not panic");
    }

    let expected = PRODUCERS * INTENTS_PER_PRODUCER;
    let tickets: Vec<_> = ticket_rx.into_iter().collect();
    assert_eq!(tickets.len(), expected);

    let mut versions = Vec::with_capacity(expected);
    for ticket in tickets {
        match ticket.wait().expect("every admitted ticket resolves") {
            TicketOutcome::Accepted { world, .. } => versions.push(world.version()),
            TicketOutcome::Rejected(error) => panic!("stress intent rejected: {error}"),
        }
    }
    controller.flush().expect("controller drains");

    versions.sort_unstable();
    let expected_versions: Vec<u64> = (1..=expected as u64).collect();
    assert_eq!(versions, expected_versions);
    assert_eq!(database.snapshot().version(), expected as u64);
    assert_eq!(database.frame_count(), expected);

    let metrics = controller.metrics();
    assert_eq!(metrics.submitted, expected as u64);
    assert_eq!(metrics.claimed, expected as u64);
    assert_eq!(metrics.accepted, expected as u64);
    assert_eq!(metrics.rejected, 0);
    assert_eq!(metrics.queue_depth, 0);
    assert_eq!(metrics.in_flight, 0);
    assert!(metrics.maximum_queue_depth <= metrics.capacity as u64);
    assert!(metrics.worker_alive);
}

#[test]
fn dropping_a_ticket_before_claim_does_not_remove_its_intent() {
    let database =
        Arc::new(Database::new(MemoryCommitStore::new()).expect("empty memory store is valid"));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let block_once = Arc::new(AtomicBool::new(true));
    let validator_release = release_rx.clone();
    let validator_once = block_once.clone();
    database.register_validator(move |_| {
        if validator_once.swap(false, Ordering::SeqCst) {
            entered_tx
                .send(())
                .expect("test observes blocked predecessor");
            validator_release
                .lock()
                .expect("release receiver lock")
                .recv()
                .expect("test releases predecessor");
        }
        Ok(())
    });

    let controller =
        QueuedIntentController::new(database.clone(), 1, 1).expect("controller starts");
    let first = controller
        .submit(QueuedIntent::new())
        .expect("first intent is claimed");
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first intent reaches validator");
    assert_eq!(first.state().phase, TicketPhase::Claimed);

    let mut queued = QueuedIntent::new();
    queued.define_fact(
        SlotId::new("queued-abandonment/state"),
        state_fact(EntityId::new(1), "committed".to_owned()),
    );
    let queued_ticket = controller.submit(queued).expect("second intent is queued");
    assert_eq!(queued_ticket.state().phase, TicketPhase::Queued);
    drop(queued_ticket);

    release_tx.send(()).expect("release first intent");
    match first.wait().expect("first intent resolves") {
        TicketOutcome::Accepted { .. } => {}
        TicketOutcome::Rejected(error) => panic!("first intent rejected: {error}"),
    }
    controller
        .flush()
        .expect("queued abandoned intent completes");

    assert_eq!(database.snapshot().version(), 2);
    assert!(
        database
            .snapshot()
            .resolve(&SlotId::new("queued-abandonment/state"))
            .is_some()
    );
    let metrics = controller.metrics();
    assert_eq!(metrics.submitted, 2);
    assert_eq!(metrics.claimed, 2);
    assert_eq!(metrics.accepted, 2);
    assert_eq!(metrics.abandoned_tickets, 1);
    assert_eq!(metrics.completion_delivery_failures, 1);
}
