use forthdb_core::{Atom, EntityId, Fact, Literal, Predicate, SlotId};
use forthdb_world::{
    Database, MemoryCommitStore, QueuedIntent, QueuedIntentController, SubmitError, TicketOutcome,
};
use std::sync::{mpsc, Arc, Barrier};
use std::thread;

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
    let database = Arc::new(
        Database::new(MemoryCommitStore::new()).expect("empty memory store is valid"),
    );
    let controller = Arc::new(
        QueuedIntentController::new(database.clone(), 64, 16).expect("controller starts"),
    );
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
