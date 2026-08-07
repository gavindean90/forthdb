#[cfg(target_os = "linux")]
mod linux {
    use forthdb_core::{
        Atom, BoundValue, EntityId, ForthDb, Literal, Pattern, Predicate, PredicateTerm,
        QueryOptions, Symbol, Term, Variable,
    };
    use forthdb_world::{
        AdmissionEpochBatchSubmitError, AdmissionEpochController, AdmissionEpochMetrics,
        AdmissionEpochSubmitError, AdmissionEpochTicket, AdmissionEpochTicketOutcome, IntentFact,
        QueuedIntent, TempEntity, World,
    };
    use serde::Serialize;
    use std::collections::BTreeMap;
    use std::error::Error;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Instant;

    const NAMESPACE: &str = "library";
    const CAPACITY: usize = 64;
    const MAX_BATCH: usize = 16;
    const RING_ENTRIES: u32 = 64;

    #[derive(Clone, Copy)]
    enum Materializer {
        TokenVm,
        World,
    }

    impl Materializer {
        fn from_environment() -> Result<Self, Box<dyn Error>> {
            match std::env::var("FORTHDB_LIBRARY_MATERIALIZER")
                .unwrap_or_else(|_| "vm".to_owned())
                .as_str()
            {
                "vm" => Ok(Self::TokenVm),
                "world" => Ok(Self::World),
                value => Err(format!(
                    "FORTHDB_LIBRARY_MATERIALIZER must be vm or world, found {value}"
                )
                .into()),
            }
        }

        fn label(self) -> &'static str {
            match self {
                Self::TokenVm => "io_uring_admission_journal_vm_root",
                Self::World => "io_uring_admission_journal_epoch_worlds",
            }
        }

        fn uses_vm(self) -> bool {
            matches!(self, Self::TokenVm)
        }

        fn open(self, path: &Path) -> Result<AdmissionEpochController, Box<dyn Error>> {
            Ok(match self {
                Self::TokenVm => {
                    AdmissionEpochController::open_vm(path, CAPACITY, MAX_BATCH, RING_ENTRIES)?
                }
                Self::World => {
                    AdmissionEpochController::open(path, CAPACITY, MAX_BATCH, RING_ENTRIES)?
                }
            })
        }
    }

    #[derive(Serialize)]
    struct Report {
        status: &'static str,
        engine: &'static str,
        database_path: String,
        elapsed_us: u128,
        final_query_projection_was_deferred: bool,
        final_query_projection_elapsed_us: u128,
        final_legacy_query_projection_materialized: bool,
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
        open_elapsed_us: u128,
        query_projection_was_deferred: bool,
        query_projection_elapsed_us: u128,
        legacy_query_projection_materialized: bool,
        same_world: bool,
        same_version: bool,
        same_frame_count: bool,
        copies_and_shelves: Vec<BTreeMap<String, String>>,
    }

    #[derive(Serialize)]
    struct ControllerObservation {
        submitted_intents: u64,
        accepted_intents: u64,
        rejected_intents: u64,
        durable_epochs: u64,
        applied_epochs: u64,
        published_worlds: u64,
        maximum_semantic_lag: u64,
        admitted_bytes: u64,
        data_writes: u64,
        data_syncs: u64,
        completion_events: u64,
        vm_materialized_epochs: u64,
        world_materialized_epochs: u64,
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
        let report = run_library(&path, Materializer::from_environment()?)?;
        let json = serde_json::to_string_pretty(&report)?;
        if let Ok(report_path) = std::env::var("FORTHDB_LIBRARY_REPORT") {
            fs::write(report_path, format!("{json}\n"))?;
        }
        println!("{json}");
        Ok(())
    }

    fn run_library(path: &Path, materializer: Materializer) -> Result<Report, Box<dyn Error>> {
        let started = Instant::now();
        let controller = materializer.open(path)?;

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
        let mut foundation_epoch = submit_epoch(&controller, vec![metadata, catalog])?;
        let metadata_world = wait_accepted(foundation_epoch.remove(0))?.world;
        let catalog_world = wait_accepted(foundation_epoch.remove(0))?.world;
        if metadata_world.id() != catalog_world.id() {
            return Err("metadata and catalog did not publish as one epoch world".into());
        }

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

        let mut circulation_epoch = submit_epoch(&controller, vec![checkout, relocate])?;
        let checkout_world = wait_accepted(circulation_epoch.remove(0))?.world;
        let relocated_world = wait_accepted(circulation_epoch.remove(0))?.world;
        if checkout_world.id() != relocated_world.id() {
            return Err("checkout and relocation did not publish as one epoch world".into());
        }

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
            &catalog_world,
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

        let renamed_world =
            wait_accepted(submit_epoch(&controller, vec![rename_and_rebind])?.remove(0))?.world;
        let final_world =
            wait_accepted(submit_epoch(&controller, vec![returned])?.remove(0))?.world;
        controller.flush()?;

        let old_compiled_after_rename_and_rebind =
            query(&renamed_world, std::slice::from_ref(&compiled_alice));
        let newly_compiled_alice = Pattern::new(
            Term::Variable(variable("copy")?),
            PredicateTerm::Predicate(Predicate::new("borrowed_by")),
            Term::Atom(Atom::Entity(resolve_symbol(&renamed_world, "Alice")?)),
        );
        let new_compiled_after_symbol_rebind = query(&renamed_world, &[newly_compiled_alice]);
        let final_query_projection_was_deferred = !final_world.is_query_projection_materialized();
        let final_projection_started = Instant::now();
        final_world.materialize_query_projection();
        let final_query_projection_elapsed = final_projection_started.elapsed();
        let after_return = query(
            &final_world,
            &[pattern_entity_variable(copy_42, "borrowed_by", "patron")?],
        );

        let controller_metrics = controller.metrics();
        let observation = observation(&controller_metrics);
        let expected_world = final_world.id();
        let expected_version = final_world.version();
        let expected_frames = final_world.frames().len();
        controller.shutdown();
        drop(controller);

        let recovery_started = Instant::now();
        let recovered = materializer.open(path)?;
        let recovery_open_elapsed = recovery_started.elapsed();
        let recovered_world = recovered.snapshot();
        let recovery_query_projection_was_deferred =
            !recovered_world.is_query_projection_materialized();
        let recovery_projection_started = Instant::now();
        recovered_world.materialize_query_projection();
        let recovery_query_projection_elapsed = recovery_projection_started.elapsed();
        let recovered_locations = query(&recovered_world, &copies_and_shelves);
        let recovery = Recovery {
            open_elapsed_us: recovery_open_elapsed.as_micros(),
            query_projection_was_deferred: recovery_query_projection_was_deferred,
            query_projection_elapsed_us: recovery_query_projection_elapsed.as_micros(),
            legacy_query_projection_materialized: recovered_world
                .is_legacy_query_projection_materialized(),
            same_world: recovered_world.id() == expected_world,
            same_version: recovered_world.version() == expected_version,
            same_frame_count: recovered_world.frames().len() == expected_frames,
            copies_and_shelves: recovered_locations,
        };

        if !recovery.same_world || !recovery.same_version || !recovery.same_frame_count {
            return Err("reopened library did not reconstruct the durable world".into());
        }
        if after_return != Vec::<BTreeMap<String, String>>::new() {
            return Err("returned copy remained checked out".into());
        }
        if observation.durable_epochs != observation.applied_epochs {
            return Err("durable admission and semantic publication did not converge".into());
        }
        if materializer.uses_vm()
            && (observation.vm_materialized_epochs != observation.applied_epochs
                || observation.world_materialized_epochs != 0)
        {
            return Err("library epochs did not remain on the token VM materializer".into());
        }

        Ok(Report {
            status: "ok",
            engine: materializer.label(),
            database_path: path.display().to_string(),
            elapsed_us: started.elapsed().as_micros(),
            final_query_projection_was_deferred,
            final_query_projection_elapsed_us: final_query_projection_elapsed.as_micros(),
            final_legacy_query_projection_materialized: final_world
                .is_legacy_query_projection_materialized(),
            world_version: recovered_world.version(),
            world_id: recovered_world.id().to_string(),
            frame_count: recovered_world.frames().len(),
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
        controller: &AdmissionEpochController,
        mut intent: QueuedIntent,
    ) -> Result<AdmissionEpochTicket, Box<dyn Error>> {
        loop {
            match controller.submit(intent) {
                Ok(ticket) => return Ok(ticket),
                Err(AdmissionEpochSubmitError::Full(returned)) => {
                    intent = returned;
                    std::thread::yield_now();
                }
                Err(AdmissionEpochSubmitError::Closed(_)) => return Err("controller closed".into()),
            }
        }
    }

    fn submit_epoch(
        controller: &AdmissionEpochController,
        mut intents: Vec<QueuedIntent>,
    ) -> Result<Vec<AdmissionEpochTicket>, Box<dyn Error>> {
        loop {
            match controller.submit_epoch(intents) {
                Ok(tickets) => return Ok(tickets),
                Err(AdmissionEpochBatchSubmitError::Full(returned)) => {
                    intents = returned;
                    std::thread::yield_now();
                }
                Err(AdmissionEpochBatchSubmitError::Closed(_)) => {
                    return Err("controller closed".into());
                }
            }
        }
    }

    fn wait_accepted(ticket: AdmissionEpochTicket) -> Result<Accepted, Box<dyn Error>> {
        let admitted = ticket.wait_admitted()?;
        match ticket.wait()? {
            AdmissionEpochTicketOutcome::Accepted {
                receipt,
                world,
                entities,
            } if receipt == admitted => Ok(Accepted { world, entities }),
            AdmissionEpochTicketOutcome::Accepted { .. } => {
                Err("admission receipt and semantic outcome disagree".into())
            }
            AdmissionEpochTicketOutcome::Rejected { error, .. } => {
                Err(format!("intent rejected: {error}").into())
            }
            AdmissionEpochTicketOutcome::Failed(error) => {
                Err(format!("admission/materialization failed: {error}").into())
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

    fn observation(controller: &AdmissionEpochMetrics) -> ControllerObservation {
        ControllerObservation {
            submitted_intents: controller.submitted_intents,
            accepted_intents: controller.accepted_intents,
            rejected_intents: controller.rejected_intents,
            durable_epochs: controller.durable_epochs,
            applied_epochs: controller.applied_epochs,
            published_worlds: controller.published_worlds,
            maximum_semantic_lag: controller.maximum_semantic_lag,
            admitted_bytes: controller.admitted_bytes,
            data_writes: controller.data_writes,
            data_syncs: controller.data_syncs,
            completion_events: controller.completion_events,
            vm_materialized_epochs: controller.vm_materialized_epochs,
            world_materialized_epochs: controller.world_materialized_epochs,
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
