#[cfg(target_os = "linux")]
mod linux {
    use forthdb_core::{
        Atom, BoundValue, EntityId, ForthDb, Literal, Pattern, Predicate, PredicateTerm,
        QueryOptions, Symbol, Term, Variable,
    };
    use forthdb_world::{
        Database, DurableCommitTicket, DurableQueuedControllerMetrics,
        DurableQueuedIntentController, DurableSubmitError, DurableTicketOutcome, FileCommitStore,
        IntentFact, IoUringEpochFileIo, QueuedIntent, TempEntity, World,
    };
    use serde::Serialize;
    use std::collections::BTreeMap;
    use std::error::Error;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    const NAMESPACE: &str = "library";
    const CAPACITY: usize = 64;
    const MAX_BATCH: usize = 1;
    const RING_ENTRIES: u32 = 64;

    #[derive(Serialize)]
    struct Report {
        status: &'static str,
        engine: &'static str,
        database_path: String,
        world_version: u64,
        world_id: String,
        frame_count: usize,
        active_slots: usize,
        immutable_records: usize,
        author: Vec<BTreeMap<String, String>>,
        copies_and_shelves_initial: Vec<BTreeMap<String, String>>,
        alice_holdings: Vec<BTreeMap<String, String>>,
        copy_87_before_move: Vec<BTreeMap<String, String>>,
        copy_87_after_move: Vec<BTreeMap<String, String>>,
        old_compiled_after_rename_and_rebind: Vec<BTreeMap<String, String>>,
        new_compiled_after_symbol_rebind: Vec<BTreeMap<String, String>>,
        after_return: Vec<BTreeMap<String, String>>,
        recovery: Recovery,
        controller: ControllerObservation,
    }

    #[derive(Serialize)]
    struct Recovery {
        same_world: bool,
        same_version: bool,
        same_frame_count: bool,
        copies_and_shelves: Vec<BTreeMap<String, String>>,
    }

    #[derive(Serialize)]
    struct ControllerObservation {
        submitted: u64,
        accepted: u64,
        epochs: u64,
        speculative_epochs_prepared: u64,
        speculative_epochs_rederived: u64,
        data_writes: u64,
        data_syncs: u64,
        completion_events: u64,
    }

    struct Accepted {
        world: Arc<World>,
        entities: BTreeMap<TempEntity, EntityId>,
    }

    pub fn main() -> Result<(), Box<dyn Error>> {
        let path = std::env::args_os()
            .nth(1)
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::temp_dir().join(format!(
                    "forthdb-library-io-uring-{}.fdb",
                    std::process::id()
                ))
            });
        let _ = fs::remove_file(&path);
        let report = run_library(&path)?;
        let json = serde_json::to_string_pretty(&report)?;
        if let Ok(report_path) = std::env::var("FORTHDB_LIBRARY_REPORT") {
            fs::write(report_path, format!("{json}\n"))?;
        }
        println!("{json}");
        Ok(())
    }

    fn run_library(path: &Path) -> Result<Report, Box<dyn Error>> {
        let controller = DurableQueuedIntentController::open_owned_speculative(
            path,
            CAPACITY,
            MAX_BATCH,
            RING_ENTRIES,
        )?;
        let database = controller.database();

        let named = [
            ("Asimov", "Isaac Asimov"),
            ("Foundation", "Foundation"),
            ("Science_Fiction", "Science Fiction"),
            ("Copy_42", "Copy 42"),
            ("Copy_87", "Copy 87"),
            ("Shelf_A3", "Shelf A3"),
            ("Shelf_B1", "Shelf B1"),
            ("Shelf_C3", "Shelf C3"),
            ("Alice", "Alice"),
            ("Bob", "Bob"),
        ];

        let mut allocation = QueuedIntent::new();
        let temporary = named
            .iter()
            .map(|(symbol, _)| ((*symbol).to_owned(), allocation.entity()))
            .collect::<BTreeMap<_, _>>();
        let allocated = wait_accepted(submit(&controller, allocation)?)?;
        let entities = temporary
            .iter()
            .map(|(name, temporary)| {
                allocated
                    .entities
                    .get(temporary)
                    .copied()
                    .map(|entity| (name.clone(), entity))
                    .ok_or_else(|| format!("allocator did not resolve {name}"))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;

        let mut metadata = QueuedIntent::new();
        for (symbol, display) in named {
            let entity = entities[symbol];
            metadata.define(
                ForthDb::display_slot(entity),
                IntentFact::new(
                    entity,
                    Predicate::new("display_name"),
                    Literal::new(display),
                ),
            );
            metadata.define(
                ForthDb::symbol_slot(NAMESPACE, &Symbol::new(symbol)),
                IntentFact::new(
                    Literal::new(format!("{NAMESPACE}:{symbol}")),
                    Predicate::new("resolves_to"),
                    entity,
                ),
            );
        }
        wait_accepted(submit(&controller, metadata)?)?;

        let asimov = entities["Asimov"];
        let foundation = entities["Foundation"];
        let science_fiction = entities["Science_Fiction"];
        let copy_42 = entities["Copy_42"];
        let copy_87 = entities["Copy_87"];
        let shelf_a3 = entities["Shelf_A3"];
        let shelf_b1 = entities["Shelf_B1"];
        let shelf_c3 = entities["Shelf_C3"];
        let alice = entities["Alice"];
        let bob = entities["Bob"];

        let mut catalog = QueuedIntent::new();
        define_relation(
            &mut catalog,
            relation_slot(foundation, "author", "current"),
            foundation,
            "written_by",
            asimov,
        );
        define_relation(
            &mut catalog,
            relation_slot(foundation, "subject", "sf"),
            foundation,
            "subject",
            science_fiction,
        );
        define_relation(
            &mut catalog,
            relation_slot(foundation, "copy", "42"),
            foundation,
            "has_copy",
            copy_42,
        );
        define_relation(
            &mut catalog,
            relation_slot(foundation, "copy", "87"),
            foundation,
            "has_copy",
            copy_87,
        );
        define_relation(
            &mut catalog,
            relation_slot(copy_42, "location", "current"),
            copy_42,
            "located_at",
            shelf_a3,
        );
        define_relation(
            &mut catalog,
            relation_slot(copy_87, "location", "current"),
            copy_87,
            "located_at",
            shelf_b1,
        );
        let catalog_world = wait_accepted(submit(&controller, catalog)?)?.world;

        let who_wrote = pattern_entity_variable(foundation, "written_by", "author")?;
        let copies_and_shelves = copies_and_shelves(foundation)?;
        let author = query(&catalog_world, &[who_wrote]);
        let copies_and_shelves_initial = query(&catalog_world, &copies_and_shelves);

        let borrower_slot = relation_slot(copy_42, "borrower", "current");
        let mut checkout = QueuedIntent::new();
        define_relation(
            &mut checkout,
            borrower_slot.clone(),
            copy_42,
            "borrowed_by",
            alice,
        );
        let mut relocate = QueuedIntent::new();
        define_relation(
            &mut relocate,
            relation_slot(copy_87, "location", "current"),
            copy_87,
            "located_at",
            shelf_c3,
        );

        // Submit both before waiting so the controller can prepare relocation
        // while checkout durability is in flight.
        let checkout_ticket = submit(&controller, checkout)?;
        let relocate_ticket = submit(&controller, relocate)?;
        let checkout_world = wait_accepted(checkout_ticket)?.world;
        let relocated_world = wait_accepted(relocate_ticket)?.world;

        let alice_holdings_pattern = vec![
            pattern_variable_entity("copy", "borrowed_by", alice)?,
            Pattern::new(
                Term::Variable(variable("work")?),
                PredicateTerm::Predicate(Predicate::new("has_copy")),
                Term::Variable(variable("copy")?),
            ),
        ];
        let alice_holdings = query(&checkout_world, &alice_holdings_pattern);
        let copy_87_before_move = query(
            &checkout_world,
            &[pattern_entity_variable(copy_87, "located_at", "shelf")?],
        );
        let copy_87_after_move = query(
            &relocated_world,
            &[pattern_entity_variable(copy_87, "located_at", "shelf")?],
        );
        let compiled_alice = Pattern::new(
            Term::Variable(variable("copy")?),
            PredicateTerm::Predicate(Predicate::new("borrowed_by")),
            Term::Atom(Atom::Entity(resolve_symbol(&relocated_world, "Alice")?)),
        );

        let mut rename_and_rebind = QueuedIntent::new();
        rename_and_rebind.define(
            ForthDb::display_slot(alice),
            IntentFact::new(
                alice,
                Predicate::new("display_name"),
                Literal::new("Alicia"),
            ),
        );
        rename_and_rebind.define(
            ForthDb::symbol_slot(NAMESPACE, &Symbol::new("Alice")),
            IntentFact::new(
                Literal::new("library:Alice"),
                Predicate::new("resolves_to"),
                bob,
            ),
        );
        let mut returned = QueuedIntent::new();
        returned.forget(borrower_slot);

        let rename_ticket = submit(&controller, rename_and_rebind)?;
        let return_ticket = submit(&controller, returned)?;
        let renamed_world = wait_accepted(rename_ticket)?.world;
        let final_world = wait_accepted(return_ticket)?.world;
        controller.flush()?;

        let old_compiled_after_rename_and_rebind =
            query(&renamed_world, std::slice::from_ref(&compiled_alice));
        let newly_compiled_alice = Pattern::new(
            Term::Variable(variable("copy")?),
            PredicateTerm::Predicate(Predicate::new("borrowed_by")),
            Term::Atom(Atom::Entity(resolve_symbol(&renamed_world, "Alice")?)),
        );
        let new_compiled_after_symbol_rebind = query(&renamed_world, &[newly_compiled_alice]);
        let after_return = query(
            &final_world,
            &[pattern_entity_variable(copy_42, "borrowed_by", "patron")?],
        );

        let controller_metrics = controller.metrics();
        let store_metrics = controller.store_metrics();
        let observation = observation(&controller_metrics, &store_metrics);
        let expected_world = final_world.id();
        let expected_version = final_world.version();
        let expected_frames = database.frame_count();
        controller.shutdown();
        drop(controller);
        drop(database);

        let recovered = Database::new(FileCommitStore::open(path)?)?;
        let recovered_world = recovered.snapshot();
        let recovered_locations = query(&recovered_world, &copies_and_shelves);
        let recovery = Recovery {
            same_world: recovered_world.id() == expected_world,
            same_version: recovered_world.version() == expected_version,
            same_frame_count: recovered.frame_count() == expected_frames,
            copies_and_shelves: recovered_locations,
        };

        if !recovery.same_world || !recovery.same_version || !recovery.same_frame_count {
            return Err("reopened library did not reconstruct the durable world".into());
        }
        if after_return != Vec::<BTreeMap<String, String>>::new() {
            return Err("returned copy remained checked out".into());
        }
        if observation.speculative_epochs_prepared == 0 {
            return Err("library workload did not exercise speculative preparation".into());
        }

        Ok(Report {
            status: "ok",
            engine: "speculative_io_uring_one_epoch_ahead",
            database_path: path.display().to_string(),
            world_version: recovered_world.version(),
            world_id: recovered_world.id().to_string(),
            frame_count: recovered.frame_count(),
            active_slots: recovered_world.active_slot_count(),
            immutable_records: recovered_world.record_count(),
            author,
            copies_and_shelves_initial,
            alice_holdings,
            copy_87_before_move,
            copy_87_after_move,
            old_compiled_after_rename_and_rebind,
            new_compiled_after_symbol_rebind,
            after_return,
            recovery,
            controller: observation,
        })
    }

    fn relation_slot(owner: EntityId, relation: &str, suffix: &str) -> forthdb_core::SlotId {
        forthdb_core::SlotId::new(format!("{}/{relation}/{suffix}", owner.value()))
    }

    fn define_relation(
        intent: &mut QueuedIntent,
        slot: forthdb_core::SlotId,
        subject: EntityId,
        predicate: &str,
        object: EntityId,
    ) {
        intent.define(
            slot,
            IntentFact::new(subject, Predicate::new(predicate), object),
        );
    }

    fn submit(
        controller: &DurableQueuedIntentController<IoUringEpochFileIo>,
        mut intent: QueuedIntent,
    ) -> Result<DurableCommitTicket, Box<dyn Error>> {
        loop {
            match controller.submit(intent) {
                Ok(ticket) => return Ok(ticket),
                Err(DurableSubmitError::Full(returned)) => {
                    intent = returned;
                    std::thread::yield_now();
                }
                Err(DurableSubmitError::Closed(_)) => return Err("controller closed".into()),
                Err(DurableSubmitError::Poisoned { reason, .. }) => {
                    return Err(format!("controller poisoned: {reason}").into());
                }
            }
        }
    }

    fn wait_accepted(ticket: DurableCommitTicket) -> Result<Accepted, Box<dyn Error>> {
        match ticket.wait()? {
            DurableTicketOutcome::Accepted {
                world, entities, ..
            } => Ok(Accepted { world, entities }),
            DurableTicketOutcome::Rejected(error) => {
                Err(format!("intent rejected: {error}").into())
            }
            DurableTicketOutcome::DurabilityFailed(error) => {
                Err(format!("durability failed: {error}").into())
            }
            DurableTicketOutcome::Stopped(reason) => {
                Err(format!("controller stopped: {reason:?}").into())
            }
        }
    }

    fn variable(name: &str) -> Result<Variable, Box<dyn Error>> {
        Ok(Variable::new(name)?)
    }

    fn pattern_entity_variable(
        subject: EntityId,
        predicate: &str,
        object: &str,
    ) -> Result<Pattern, Box<dyn Error>> {
        Ok(Pattern::new(
            Term::Atom(Atom::Entity(subject)),
            PredicateTerm::Predicate(Predicate::new(predicate)),
            Term::Variable(variable(object)?),
        ))
    }

    fn pattern_variable_entity(
        subject: &str,
        predicate: &str,
        object: EntityId,
    ) -> Result<Pattern, Box<dyn Error>> {
        Ok(Pattern::new(
            Term::Variable(variable(subject)?),
            PredicateTerm::Predicate(Predicate::new(predicate)),
            Term::Atom(Atom::Entity(object)),
        ))
    }

    fn copies_and_shelves(foundation: EntityId) -> Result<Vec<Pattern>, Box<dyn Error>> {
        Ok(vec![
            pattern_entity_variable(foundation, "has_copy", "copy")?,
            Pattern::new(
                Term::Variable(variable("copy")?),
                PredicateTerm::Predicate(Predicate::new("located_at")),
                Term::Variable(variable("shelf")?),
            ),
        ])
    }

    fn resolve_symbol(world: &World, symbol: &str) -> Result<EntityId, Box<dyn Error>> {
        let slot = ForthDb::symbol_slot(NAMESPACE, &Symbol::new(symbol));
        match world.resolve(&slot).map(|fact| &fact.object) {
            Some(Atom::Entity(entity)) => Ok(*entity),
            _ => Err(format!("unbound symbol {NAMESPACE}:{symbol}").into()),
        }
    }

    fn query(world: &World, patterns: &[Pattern]) -> Vec<BTreeMap<String, String>> {
        world
            .query(patterns, QueryOptions::default())
            .rows
            .into_iter()
            .map(|row| {
                row.binding
                    .into_iter()
                    .map(|(name, value)| (name, render(world, value)))
                    .collect()
            })
            .collect()
    }

    fn render(world: &World, value: BoundValue) -> String {
        match value {
            BoundValue::Entity(entity) => world.display_name(entity),
            BoundValue::Literal(literal) => literal.as_str().to_owned(),
            BoundValue::Predicate(predicate) => predicate.as_str().to_owned(),
        }
    }

    fn observation(
        controller: &DurableQueuedControllerMetrics,
        store: &forthdb_world::FileEpochMetrics,
    ) -> ControllerObservation {
        ControllerObservation {
            submitted: controller.submitted,
            accepted: controller.accepted,
            epochs: controller.epochs,
            speculative_epochs_prepared: controller.speculative_epochs_prepared,
            speculative_epochs_rederived: controller.speculative_epochs_rederived,
            data_writes: store.data_writes,
            data_syncs: store.data_syncs,
            completion_events: store.completion_events,
        }
    }
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the speculative io_uring library application requires Linux");
    std::process::exit(2);
}
