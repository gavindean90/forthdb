#[cfg(target_os = "linux")]
mod linux {
    use forthdb_core::{
        Atom, EntityId, Fact, ForthDb, Literal, Pattern, Predicate, PredicateTerm, QueryOptions,
        SlotId, Term, Variable,
    };
    use forthdb_world::{
        AdmissionEpochBatchSubmitError, AdmissionEpochController, AdmissionEpochMetrics,
        AdmissionEpochTicket, AdmissionEpochTicketOutcome, IntentFact, QueuedIntent, TempEntity,
        World,
    };
    use serde::Serialize;
    use std::error::Error;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    const RING_ENTRIES: u32 = 256;
    const MAX_BATCH: usize = 16;
    const QUERY_SAMPLES: usize = 7;

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
                Self::TokenVm => "io_uring_admission_journal_token_vm",
                Self::World => "io_uring_admission_journal_epoch_worlds",
            }
        }

        fn uses_vm(self) -> bool {
            matches!(self, Self::TokenVm)
        }

        fn open(
            self,
            path: &Path,
            capacity: usize,
        ) -> Result<AdmissionEpochController, Box<dyn Error>> {
            Ok(match self {
                Self::TokenVm => {
                    AdmissionEpochController::open_vm(path, capacity, MAX_BATCH, RING_ENTRIES)?
                }
                Self::World => {
                    AdmissionEpochController::open(path, capacity, MAX_BATCH, RING_ENTRIES)?
                }
            })
        }
    }

    #[derive(Clone, Copy, Serialize)]
    struct Scale {
        works: usize,
        copies: usize,
        patrons: usize,
        branches: usize,
        circulation_cycles: usize,
        intents_per_cycle: usize,
    }

    impl Scale {
        fn from_environment() -> Result<Self, Box<dyn Error>> {
            let scale = Self {
                works: setting("FORTHDB_RAMPED_WORKS", 10_000)?,
                copies: setting("FORTHDB_RAMPED_COPIES", 20_000)?,
                patrons: setting("FORTHDB_RAMPED_PATRONS", 5_000)?,
                branches: setting("FORTHDB_RAMPED_BRANCHES", 8)?,
                circulation_cycles: setting("FORTHDB_RAMPED_CYCLES", 64)?,
                intents_per_cycle: 8,
            };
            let distinct_copies = scale
                .circulation_cycles
                .checked_mul(3)
                .ok_or("circulation scale overflow")?;
            let catalog_copies = scale.works.checked_mul(2).ok_or("catalog scale overflow")?;
            if scale.works == 0
                || scale.copies < catalog_copies
                || scale.patrons < 2
                || scale.branches < 2
                || scale.circulation_cycles == 0
                || distinct_copies > scale.copies
            {
                return Err("ramped scale requires works > 0, two copies per work, two patrons, two branches, at least one cycle, and three distinct copies per cycle".into());
            }
            Ok(scale)
        }

        fn circulation_intents(self) -> usize {
            self.circulation_cycles * self.intents_per_cycle
        }
    }

    #[derive(Clone)]
    struct Entities {
        works: Vec<EntityId>,
        copies: Vec<EntityId>,
        patrons: Vec<EntityId>,
        branches: Vec<EntityId>,
    }

    struct WorkItem {
        intent: QueuedIntent,
        should_accept: bool,
    }

    #[derive(Clone, Debug, Eq, PartialEq, Serialize)]
    struct Projection {
        active_slots: usize,
        immutable_records: usize,
        checked_out_copies: usize,
        active_holds: usize,
        available_after_recovery: usize,
        located_copies: usize,
    }

    #[derive(Serialize)]
    struct LatencySummary {
        median_us: u128,
        p95_us: u128,
        p99_us: u128,
        maximum_us: u128,
    }

    #[derive(Serialize)]
    struct QueryObservation {
        name: &'static str,
        rows: usize,
        latency: LatencySummary,
    }

    #[derive(Serialize)]
    struct RecoveryObservation {
        elapsed_us: u128,
        same_world: bool,
        same_version: bool,
        same_active_slots: bool,
        same_records: bool,
        same_projection: bool,
    }

    #[derive(Serialize)]
    struct ProfileReport {
        profile: &'static str,
        epoch_width: usize,
        setup_elapsed_us: u128,
        workload_elapsed_us: u128,
        intents_per_second: f64,
        admission_latency: LatencySummary,
        semantic_latency: LatencySummary,
        submitted_intents: u64,
        accepted_intents: u64,
        rejected_intents: u64,
        durable_epochs: u64,
        published_worlds: u64,
        data_syncs: u64,
        syncs_per_intent: f64,
        maximum_semantic_lag_epochs: u64,
        vm_materialized_epochs: u64,
        world_materialized_epochs: u64,
        admitted_bytes: u64,
        active_slot_growth: usize,
        immutable_record_growth: usize,
        journal_bytes: u64,
        queries: Vec<QueryObservation>,
        projection: Projection,
        recovery: RecoveryObservation,
    }

    #[derive(Serialize)]
    struct Report {
        status: &'static str,
        engine: &'static str,
        purpose: &'static str,
        scale: Scale,
        interactive: ProfileReport,
        branch_rush: ProfileReport,
        semantic_projection_equal: bool,
        branch_rush_throughput_ratio: f64,
        branch_rush_sync_reduction_percent: f64,
    }

    pub fn main() -> Result<(), Box<dyn Error>> {
        let root = std::env::args_os()
            .nth(1)
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::temp_dir().join(format!("forthdb-ramped-library-{}", std::process::id()))
            });
        fs::create_dir_all(&root)?;
        let scale = Scale::from_environment()?;
        let materializer = Materializer::from_environment()?;
        let interactive = run_profile(
            &root.join("interactive.fdb"),
            scale,
            1,
            "interactive",
            materializer,
        )?;
        let branch_rush = run_profile(
            &root.join("branch-rush.fdb"),
            scale,
            MAX_BATCH,
            "branch_rush",
            materializer,
        )?;
        let semantic_projection_equal = interactive.projection == branch_rush.projection;
        if !semantic_projection_equal {
            return Err("interactive and branch-rush projections differ".into());
        }
        let report = Report {
            status: "ok",
            engine: materializer.label(),
            purpose: "realistic scaled library circulation comparison",
            scale,
            branch_rush_throughput_ratio: branch_rush.intents_per_second
                / interactive.intents_per_second,
            branch_rush_sync_reduction_percent: 100.0
                * (1.0 - branch_rush.syncs_per_intent / interactive.syncs_per_intent),
            interactive,
            branch_rush,
            semantic_projection_equal,
        };
        let json = serde_json::to_string_pretty(&report)?;
        if let Ok(path) = std::env::var("FORTHDB_RAMPED_REPORT") {
            fs::write(path, format!("{json}\n"))?;
        }
        println!("{json}");
        Ok(())
    }

    fn run_profile(
        path: &Path,
        scale: Scale,
        epoch_width: usize,
        profile: &'static str,
        materializer: Materializer,
    ) -> Result<ProfileReport, Box<dyn Error>> {
        let _ = fs::remove_file(path);
        let capacity = scale
            .circulation_intents()
            .div_ceil(epoch_width)
            .saturating_add(8);
        let controller = materializer.open(path, capacity)?;

        let setup_started = Instant::now();
        let entities = allocate_entities(&controller, scale)?;
        seed_catalog(&controller, scale, &entities)?;
        controller.flush()?;
        let setup_elapsed = setup_started.elapsed();
        let setup_metrics = controller.metrics();
        let setup_world = controller.snapshot();
        let setup_active_slots = setup_world.active_slot_count();
        let setup_records = setup_world.record_count();

        let workload = circulation_workload(scale, &entities);
        let workload_started = Instant::now();
        let (admission_latencies, semantic_latencies, accepted, rejected) =
            execute_workload(&controller, workload, epoch_width)?;
        controller.flush()?;
        let workload_elapsed = workload_started.elapsed();
        let final_metrics = controller.metrics();
        let metrics = metric_delta(final_metrics, setup_metrics);
        if materializer.uses_vm()
            && (metrics.vm_materialized_epochs != metrics.applied_epochs
                || metrics.world_materialized_epochs != 0)
        {
            return Err(format!("{profile} left the token VM materializer").into());
        }
        if accepted as u64 != metrics.accepted_intents
            || rejected as u64 != metrics.rejected_intents
            || rejected != scale.circulation_cycles
        {
            return Err(format!(
                "unexpected circulation outcomes: accepted={accepted}, rejected={rejected}"
            )
            .into());
        }

        let world = controller.snapshot();
        let live_projection = projection(&world)?;
        if live_projection.checked_out_copies != 0
            || live_projection.active_holds != scale.circulation_cycles
            || live_projection.available_after_recovery != scale.circulation_cycles
            || live_projection.located_copies != scale.copies
        {
            return Err(
                format!("invalid final circulation projection: {live_projection:?}").into(),
            );
        }
        let queries = observe_queries(&world)?;
        let expected_world = world.id();
        let expected_version = world.version();
        let expected_slots = world.active_slot_count();
        let expected_records = world.record_count();
        controller.shutdown();
        drop(controller);

        let recovery_started = Instant::now();
        let recovered = materializer.open(path, capacity)?;
        let recovered_world = recovered.snapshot();
        let recovered_projection = projection(&recovered_world)?;
        let recovery = RecoveryObservation {
            elapsed_us: recovery_started.elapsed().as_micros(),
            same_world: recovered_world.id() == expected_world,
            same_version: recovered_world.version() == expected_version,
            same_active_slots: recovered_world.active_slot_count() == expected_slots,
            same_records: recovered_world.record_count() == expected_records,
            same_projection: recovered_projection == live_projection,
        };
        recovered.shutdown();
        drop(recovered);
        if !recovery.same_world
            || !recovery.same_version
            || !recovery.same_active_slots
            || !recovery.same_records
            || !recovery.same_projection
        {
            return Err(format!("{profile} recovery did not reproduce the live world").into());
        }

        let intent_count = scale.circulation_intents() as f64;
        Ok(ProfileReport {
            profile,
            epoch_width,
            setup_elapsed_us: setup_elapsed.as_micros(),
            workload_elapsed_us: workload_elapsed.as_micros(),
            intents_per_second: intent_count / workload_elapsed.as_secs_f64(),
            admission_latency: summarize_latencies(admission_latencies),
            semantic_latency: summarize_latencies(semantic_latencies),
            submitted_intents: metrics.submitted_intents,
            accepted_intents: metrics.accepted_intents,
            rejected_intents: metrics.rejected_intents,
            durable_epochs: metrics.durable_epochs,
            published_worlds: metrics.published_worlds,
            data_syncs: metrics.data_syncs,
            syncs_per_intent: metrics.data_syncs as f64 / intent_count,
            maximum_semantic_lag_epochs: final_metrics.maximum_semantic_lag,
            vm_materialized_epochs: metrics.vm_materialized_epochs,
            world_materialized_epochs: metrics.world_materialized_epochs,
            admitted_bytes: metrics.admitted_bytes,
            active_slot_growth: expected_slots - setup_active_slots,
            immutable_record_growth: expected_records - setup_records,
            journal_bytes: fs::metadata(path)?.len(),
            queries,
            projection: live_projection,
            recovery,
        })
    }

    fn allocate_entities(
        controller: &AdmissionEpochController,
        scale: Scale,
    ) -> Result<Entities, Box<dyn Error>> {
        let mut allocation = QueuedIntent::new();
        let work_temps = (0..scale.works)
            .map(|_| allocation.entity())
            .collect::<Vec<_>>();
        let copy_temps = (0..scale.copies)
            .map(|_| allocation.entity())
            .collect::<Vec<_>>();
        let patron_temps = (0..scale.patrons)
            .map(|_| allocation.entity())
            .collect::<Vec<_>>();
        let branch_temps = (0..scale.branches)
            .map(|_| allocation.entity())
            .collect::<Vec<_>>();
        let accepted = wait_accepted(
            submit_epoch(controller, vec![allocation])?
                .pop()
                .expect("allocation epoch has one ticket"),
        )?;
        Ok(Entities {
            works: resolve_entities(&accepted.entities, work_temps)?,
            copies: resolve_entities(&accepted.entities, copy_temps)?,
            patrons: resolve_entities(&accepted.entities, patron_temps)?,
            branches: resolve_entities(&accepted.entities, branch_temps)?,
        })
    }

    fn seed_catalog(
        controller: &AdmissionEpochController,
        scale: Scale,
        entities: &Entities,
    ) -> Result<(), Box<dyn Error>> {
        let mut catalog = QueuedIntent::new();
        for (index, entity) in entities.works.iter().copied().enumerate() {
            display(&mut catalog, entity, format!("Work {index:05}"));
            for ordinal in 0..2 {
                let copy = entities.copies[index * 2 + ordinal];
                relation(
                    &mut catalog,
                    relation_slot(entity, "copy", &ordinal.to_string()),
                    entity,
                    "has_copy",
                    copy,
                );
            }
        }
        for (index, entity) in entities.copies.iter().copied().enumerate() {
            display(&mut catalog, entity, format!("Copy {index:05}"));
            relation(
                &mut catalog,
                relation_slot(entity, "location", "current"),
                entity,
                "located_at",
                entities.branches[index % scale.branches],
            );
        }
        for (index, entity) in entities.patrons.iter().copied().enumerate() {
            display(&mut catalog, entity, format!("Patron {index:05}"));
        }
        for (index, entity) in entities.branches.iter().copied().enumerate() {
            display(&mut catalog, entity, format!("Branch {index:02}"));
        }
        wait_accepted(
            submit_epoch(controller, vec![catalog])?
                .pop()
                .expect("catalog epoch has one ticket"),
        )?;
        Ok(())
    }

    fn circulation_workload(scale: Scale, entities: &Entities) -> Vec<WorkItem> {
        let mut workload = Vec::with_capacity(scale.circulation_intents());
        for cycle in 0..scale.circulation_cycles {
            let checkout_copy = entities.copies[cycle % entities.copies.len()];
            let moved_copy =
                entities.copies[(cycle + scale.circulation_cycles) % entities.copies.len()];
            let recovered_copy =
                entities.copies[(cycle + 2 * scale.circulation_cycles) % entities.copies.len()];
            let patron = entities.patrons[cycle % entities.patrons.len()];
            let contender = entities.patrons[(cycle + 1) % entities.patrons.len()];
            let borrower_slot = relation_slot(checkout_copy, "borrower", "current");
            let borrower_fact = Fact::new(
                Atom::Entity(checkout_copy),
                Predicate::new("borrowed_by"),
                Atom::Entity(patron),
            );

            let mut checkout = QueuedIntent::new();
            checkout.expect_absent(borrower_slot.clone());
            checkout.define_fact(borrower_slot.clone(), borrower_fact.clone());
            workload.push(WorkItem {
                intent: checkout,
                should_accept: true,
            });

            let mut contested = QueuedIntent::new();
            contested.expect_absent(borrower_slot.clone());
            contested.define(
                borrower_slot.clone(),
                IntentFact::new(checkout_copy, Predicate::new("borrowed_by"), contender),
            );
            workload.push(WorkItem {
                intent: contested,
                should_accept: false,
            });

            let mut hold = QueuedIntent::new();
            relation(
                &mut hold,
                relation_slot(
                    entities.works[cycle % entities.works.len()],
                    "hold",
                    &cycle.to_string(),
                ),
                entities.works[cycle % entities.works.len()],
                "held_by",
                patron,
            );
            workload.push(WorkItem {
                intent: hold,
                should_accept: true,
            });

            let mut move_copy = QueuedIntent::new();
            relation(
                &mut move_copy,
                relation_slot(moved_copy, "location", "current"),
                moved_copy,
                "located_at",
                entities.branches[(cycle + 1) % entities.branches.len()],
            );
            workload.push(WorkItem {
                intent: move_copy,
                should_accept: true,
            });

            let state_slot = relation_slot(recovered_copy, "state", "current");
            let lost_fact = Fact::new(
                Atom::Entity(recovered_copy),
                Predicate::new("circulation_state"),
                Atom::Literal(Literal::new("lost")),
            );
            let mut lost = QueuedIntent::new();
            lost.define_fact(state_slot.clone(), lost_fact.clone());
            workload.push(WorkItem {
                intent: lost,
                should_accept: true,
            });

            let mut recovered = QueuedIntent::new();
            recovered.expect_value(state_slot.clone(), lost_fact);
            recovered.define(
                state_slot,
                IntentFact::new(
                    recovered_copy,
                    Predicate::new("circulation_state"),
                    Literal::new("available"),
                ),
            );
            workload.push(WorkItem {
                intent: recovered,
                should_accept: true,
            });

            let mut rename = QueuedIntent::new();
            display(&mut rename, patron, format!("Patron {cycle:05} renamed"));
            workload.push(WorkItem {
                intent: rename,
                should_accept: true,
            });

            let mut returned = QueuedIntent::new();
            returned.expect_value(borrower_slot.clone(), borrower_fact);
            returned.forget(borrower_slot);
            workload.push(WorkItem {
                intent: returned,
                should_accept: true,
            });
        }
        workload
    }

    fn execute_workload(
        controller: &AdmissionEpochController,
        workload: Vec<WorkItem>,
        epoch_width: usize,
    ) -> Result<(Vec<Duration>, Vec<Duration>, usize, usize), Box<dyn Error>> {
        if epoch_width == 1 {
            let mut admitted = Vec::with_capacity(workload.len());
            let mut semantic = Vec::with_capacity(workload.len());
            let mut accepted = 0;
            let mut rejected = 0;
            for item in workload {
                let started = Instant::now();
                let ticket = submit_epoch(controller, vec![item.intent])?
                    .pop()
                    .expect("interactive epoch has one ticket");
                ticket.wait_admitted()?;
                admitted.push(started.elapsed());
                observe_outcome(ticket, item.should_accept, &mut accepted, &mut rejected)?;
                semantic.push(started.elapsed());
            }
            return Ok((admitted, semantic, accepted, rejected));
        }

        let mut pending = Vec::new();
        let mut iterator = workload.into_iter();
        loop {
            let group = iterator.by_ref().take(epoch_width).collect::<Vec<_>>();
            if group.is_empty() {
                break;
            }
            let expected = group
                .iter()
                .map(|item| item.should_accept)
                .collect::<Vec<_>>();
            let intents = group.into_iter().map(|item| item.intent).collect();
            let started = Instant::now();
            pending.push((started, submit_epoch(controller, intents)?, expected));
        }

        let mut admission = Vec::new();
        let mut semantic = Vec::new();
        let mut accepted = 0;
        let mut rejected = 0;
        for (started, tickets, expected) in pending {
            let ticket_count = tickets.len();
            for ticket in &tickets {
                ticket.wait_admitted()?;
            }
            let admission_elapsed = started.elapsed();
            admission.extend(std::iter::repeat_n(admission_elapsed, ticket_count));
            for (ticket, should_accept) in tickets.into_iter().zip(expected) {
                observe_outcome(ticket, should_accept, &mut accepted, &mut rejected)?;
            }
            let semantic_elapsed = started.elapsed();
            semantic.extend(std::iter::repeat_n(semantic_elapsed, ticket_count));
        }
        if admission.len() != accepted + rejected || semantic.len() != admission.len() {
            return Err("branch-rush ticket accounting mismatch".into());
        }
        Ok((admission, semantic, accepted, rejected))
    }

    fn observe_outcome(
        ticket: AdmissionEpochTicket,
        should_accept: bool,
        accepted: &mut usize,
        rejected: &mut usize,
    ) -> Result<(), Box<dyn Error>> {
        match ticket.wait()? {
            AdmissionEpochTicketOutcome::Accepted { .. } if should_accept => *accepted += 1,
            AdmissionEpochTicketOutcome::Rejected { .. } if !should_accept => *rejected += 1,
            AdmissionEpochTicketOutcome::Failed(error) => {
                return Err(format!("admission/materialization failed: {error}").into());
            }
            outcome => return Err(format!("unexpected semantic outcome: {outcome:?}").into()),
        }
        Ok(())
    }

    struct Accepted {
        entities: std::collections::BTreeMap<TempEntity, EntityId>,
    }

    fn wait_accepted(ticket: AdmissionEpochTicket) -> Result<Accepted, Box<dyn Error>> {
        let admitted = ticket.wait_admitted()?;
        match ticket.wait()? {
            AdmissionEpochTicketOutcome::Accepted {
                receipt, entities, ..
            } if receipt == admitted => Ok(Accepted { entities }),
            outcome => Err(format!("setup intent was not accepted: {outcome:?}").into()),
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
                    return Err("admission controller closed".into());
                }
            }
        }
    }

    fn resolve_entities(
        resolved: &std::collections::BTreeMap<TempEntity, EntityId>,
        temporary: Vec<TempEntity>,
    ) -> Result<Vec<EntityId>, Box<dyn Error>> {
        temporary
            .into_iter()
            .map(|temporary| {
                resolved
                    .get(&temporary)
                    .copied()
                    .ok_or_else(|| "allocator omitted a temporary entity".into())
            })
            .collect()
    }

    fn relation_slot(owner: EntityId, relation: &str, suffix: &str) -> SlotId {
        SlotId::new(format!("{}/{relation}/{suffix}", owner.value()))
    }

    fn relation(
        intent: &mut QueuedIntent,
        slot: SlotId,
        subject: EntityId,
        predicate: &str,
        object: EntityId,
    ) {
        intent.define(
            slot,
            IntentFact::new(subject, Predicate::new(predicate), object),
        );
    }

    fn display(intent: &mut QueuedIntent, entity: EntityId, value: String) {
        intent.define(
            ForthDb::display_slot(entity),
            IntentFact::new(entity, Predicate::new("display_name"), Literal::new(value)),
        );
    }

    fn projection(world: &World) -> Result<Projection, Box<dyn Error>> {
        Ok(Projection {
            active_slots: world.active_slot_count(),
            immutable_records: world.record_count(),
            checked_out_copies: query_count(world, "borrowed_by")?,
            active_holds: query_count(world, "held_by")?,
            available_after_recovery: query_literal_count(world, "circulation_state", "available")?,
            located_copies: query_count(world, "located_at")?,
        })
    }

    fn observe_queries(world: &World) -> Result<Vec<QueryObservation>, Box<dyn Error>> {
        let queries = [
            ("all_copy_locations", "located_at", None),
            ("active_holds", "held_by", None),
            (
                "available_after_recovery",
                "circulation_state",
                Some("available"),
            ),
        ];
        queries
            .into_iter()
            .map(|(name, predicate, literal)| {
                let mut samples = Vec::with_capacity(QUERY_SAMPLES);
                let mut rows = 0;
                for _ in 0..QUERY_SAMPLES {
                    let started = Instant::now();
                    rows = match literal {
                        Some(value) => query_literal_count(world, predicate, value)?,
                        None => query_count(world, predicate)?,
                    };
                    samples.push(started.elapsed());
                }
                Ok(QueryObservation {
                    name,
                    rows,
                    latency: summarize_latencies(samples),
                })
            })
            .collect()
    }

    fn query_count(world: &World, predicate: &str) -> Result<usize, Box<dyn Error>> {
        let pattern = Pattern::new(
            Term::Variable(Variable::new("subject")?),
            PredicateTerm::Predicate(Predicate::new(predicate)),
            Term::Variable(Variable::new("object")?),
        );
        Ok(world.query(&[pattern], QueryOptions::default()).rows.len())
    }

    fn query_literal_count(
        world: &World,
        predicate: &str,
        literal: &str,
    ) -> Result<usize, Box<dyn Error>> {
        let pattern = Pattern::new(
            Term::Variable(Variable::new("subject")?),
            PredicateTerm::Predicate(Predicate::new(predicate)),
            Term::Atom(Atom::Literal(Literal::new(literal))),
        );
        Ok(world.query(&[pattern], QueryOptions::default()).rows.len())
    }

    fn metric_delta(
        after: AdmissionEpochMetrics,
        before: AdmissionEpochMetrics,
    ) -> AdmissionEpochMetrics {
        AdmissionEpochMetrics {
            submitted_intents: after.submitted_intents - before.submitted_intents,
            backpressured_intents: after.backpressured_intents - before.backpressured_intents,
            durable_epochs: after.durable_epochs - before.durable_epochs,
            applied_epochs: after.applied_epochs - before.applied_epochs,
            published_worlds: after.published_worlds - before.published_worlds,
            accepted_intents: after.accepted_intents - before.accepted_intents,
            rejected_intents: after.rejected_intents - before.rejected_intents,
            admitted_bytes: after.admitted_bytes - before.admitted_bytes,
            data_writes: after.data_writes - before.data_writes,
            data_syncs: after.data_syncs - before.data_syncs,
            completion_events: after.completion_events - before.completion_events,
            maximum_semantic_lag: after.maximum_semantic_lag,
            vm_materialized_epochs: after.vm_materialized_epochs - before.vm_materialized_epochs,
            world_materialized_epochs: after.world_materialized_epochs
                - before.world_materialized_epochs,
        }
    }

    fn summarize_latencies(mut values: Vec<Duration>) -> LatencySummary {
        values.sort_unstable();
        LatencySummary {
            median_us: percentile(&values, 50).as_micros(),
            p95_us: percentile(&values, 95).as_micros(),
            p99_us: percentile(&values, 99).as_micros(),
            maximum_us: values.last().copied().unwrap_or_default().as_micros(),
        }
    }

    fn percentile(values: &[Duration], percentile: usize) -> Duration {
        if values.is_empty() {
            return Duration::ZERO;
        }
        let index = (values.len() - 1) * percentile / 100;
        values[index]
    }

    fn setting(name: &str, default: usize) -> Result<usize, Box<dyn Error>> {
        match std::env::var(name) {
            Ok(value) => Ok(value.parse()?),
            Err(std::env::VarError::NotPresent) => Ok(default),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the ramped io_uring library application requires Linux");
    std::process::exit(2);
}
