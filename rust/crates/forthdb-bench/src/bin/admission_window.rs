#[cfg(target_os = "linux")]
mod linux {
    use forthdb_core::{Literal, Predicate, SlotId};
    use forthdb_world::{
        AdmissionEpochBatchSubmitError, AdmissionEpochController, AdmissionEpochMetrics,
        AdmissionEpochOpenError, AdmissionEpochTicket, AdmissionEpochTicketOutcome, IntentFact,
        QueuedIntent, Validator,
    };
    use serde::Serialize;
    use std::error::Error;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    const RING_ENTRIES: u32 = 256;
    const MAX_BATCH: usize = 256;
    const STEADY_EPOCHS: usize = 64;

    #[derive(Serialize)]
    struct Report {
        status: &'static str,
        scope: &'static str,
        steady_epochs_per_case: usize,
        largest_verified_backlog_epochs: usize,
        cases: Vec<CaseReport>,
        availability_error: Option<String>,
        environment: Environment,
    }

    #[derive(Serialize)]
    struct Environment {
        os: &'static str,
        architecture: &'static str,
        profile: &'static str,
        git_sha: Option<String>,
        github_run_id: Option<String>,
    }

    #[derive(Serialize)]
    struct CaseReport {
        label: String,
        max_unapplied_epochs: usize,
        intents_per_epoch: usize,
        journal_ceiling: JournalCeiling,
        steady_state: SteadyState,
        final_world_version: u64,
        final_active_slots: usize,
        journal_bytes: u64,
    }

    #[derive(Serialize)]
    struct JournalCeiling {
        epochs: usize,
        intents: usize,
        elapsed_us: u128,
        intents_per_second: f64,
        admitted_megabytes_per_second: f64,
        durable_epochs: u64,
        applied_epochs_while_gated: u64,
        published_worlds_while_gated: u64,
        durable_to_applied_lag: u64,
        provisional_world_visible: bool,
    }

    #[derive(Serialize)]
    struct SteadyState {
        epochs: usize,
        intents: usize,
        admission_elapsed_us: u128,
        publication_elapsed_us: u128,
        catch_up_elapsed_us: u128,
        admission_intents_per_second: f64,
        publication_intents_per_second: f64,
        lag_when_admission_completed: u64,
        maximum_durable_to_applied_lag: u64,
        durable_epochs: u64,
        applied_epochs: u64,
        published_worlds: u64,
        data_syncs: u64,
        syncs_per_intent: f64,
        backpressured_intents: u64,
    }

    struct Gate {
        released: Mutex<bool>,
        ready: Condvar,
    }

    impl Gate {
        fn new() -> Self {
            Self {
                released: Mutex::new(false),
                ready: Condvar::new(),
            }
        }

        fn wait(&self) {
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.ready.wait(released).unwrap();
            }
        }

        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.ready.notify_all();
        }
    }

    pub fn main() -> Result<(), Box<dyn Error>> {
        let root = std::env::args_os()
            .nth(1)
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::temp_dir()
                    .join(format!("forthdb-admission-window-{}", std::process::id()))
            });
        fs::create_dir_all(&root)?;

        let configurations = [
            (1, 16, "window-1"),
            (2, 16, "window-2"),
            (8, 16, "window-8"),
            (32, 16, "window-32"),
            (8, 1, "epoch-width-1"),
            (8, 64, "epoch-width-64"),
            (8, 128, "epoch-width-128"),
            (8, 256, "epoch-width-256"),
        ];
        let mut cases = Vec::new();
        let mut availability_error = None;
        for (window, width, label) in configurations {
            match run_case(&root.join(format!("{label}.fdb")), window, width, label) {
                Ok(case) => cases.push(case),
                Err(error) if unavailable(error.as_ref()) => {
                    availability_error = Some(error.to_string());
                    cases.clear();
                    break;
                }
                Err(error) => return Err(error),
            }
        }

        let report = Report {
            status: if availability_error.is_some() {
                "unavailable"
            } else {
                "observational"
            },
            scope: "bounded-durable-admission-versus-semantic-publication",
            steady_epochs_per_case: STEADY_EPOCHS,
            largest_verified_backlog_epochs: cases
                .iter()
                .map(|case| case.journal_ceiling.durable_to_applied_lag as usize)
                .max()
                .unwrap_or(0),
            cases,
            availability_error,
            environment: Environment {
                os: std::env::consts::OS,
                architecture: std::env::consts::ARCH,
                profile: if cfg!(debug_assertions) {
                    "debug"
                } else {
                    "release"
                },
                git_sha: std::env::var("GITHUB_SHA").ok(),
                github_run_id: std::env::var("GITHUB_RUN_ID").ok(),
            },
        };
        let json = serde_json::to_string_pretty(&report)?;
        if let Ok(path) = std::env::var("FORTHDB_ADMISSION_WINDOW_REPORT") {
            fs::write(path, format!("{json}\n"))?;
        }
        println!("{json}");
        Ok(())
    }

    fn run_case(
        path: &Path,
        window: usize,
        width: usize,
        label: &str,
    ) -> Result<CaseReport, Box<dyn Error>> {
        let _ = fs::remove_file(path);
        let gate = Arc::new(Gate::new());
        let validator_gate = gate.clone();
        let validator: Validator = Arc::new(move |_| {
            validator_gate.wait();
            Ok(())
        });
        let controller = AdmissionEpochController::open_with_validators_and_window(
            path,
            STEADY_EPOCHS + window + 8,
            MAX_BATCH,
            RING_ENTRIES,
            window,
            vec![validator],
        )?;

        let ceiling_before = controller.metrics();
        let ceiling_started = Instant::now();
        let ceiling_tickets = submit_epochs(&controller, 0, window, width)?;
        wait_admitted(&ceiling_tickets)?;
        let ceiling_elapsed = ceiling_started.elapsed();
        let gated_metrics = delta(controller.metrics(), ceiling_before);
        let gated_world = controller.snapshot();
        let ceiling_intents = window * width;
        let ceiling_bytes = gated_metrics.admitted_bytes as f64;
        let journal_ceiling = JournalCeiling {
            epochs: window,
            intents: ceiling_intents,
            elapsed_us: ceiling_elapsed.as_micros(),
            intents_per_second: rate(ceiling_intents, ceiling_elapsed),
            admitted_megabytes_per_second: ceiling_bytes
                / (1024.0 * 1024.0)
                / ceiling_elapsed.as_secs_f64(),
            durable_epochs: gated_metrics.durable_epochs,
            applied_epochs_while_gated: gated_metrics.applied_epochs,
            published_worlds_while_gated: gated_metrics.published_worlds,
            durable_to_applied_lag: controller
                .metrics()
                .durable_epochs
                .saturating_sub(controller.metrics().applied_epochs),
            provisional_world_visible: gated_world.version() != 0,
        };
        if journal_ceiling.durable_epochs != window as u64
            || journal_ceiling.applied_epochs_while_gated != 0
            || journal_ceiling.published_worlds_while_gated != 0
            || journal_ceiling.durable_to_applied_lag != window as u64
            || journal_ceiling.provisional_world_visible
        {
            return Err(format!("{label} did not preserve the gated admission boundary").into());
        }

        gate.release();
        wait_outcomes(ceiling_tickets)?;
        controller.flush()?;

        let steady_before = controller.metrics();
        let steady_started = Instant::now();
        let steady_tickets = submit_epochs(&controller, window, STEADY_EPOCHS, width)?;
        wait_admitted(&steady_tickets)?;
        let admission_elapsed = steady_started.elapsed();
        let admission_metrics = controller.metrics();
        let lag_when_admission_completed = admission_metrics
            .durable_epochs
            .saturating_sub(admission_metrics.applied_epochs);
        wait_outcomes(steady_tickets)?;
        controller.flush()?;
        let publication_elapsed = steady_started.elapsed();
        let catch_up_elapsed = publication_elapsed.saturating_sub(admission_elapsed);
        let final_metrics = controller.metrics();
        let steady_metrics = delta(final_metrics, steady_before);
        let steady_intents = STEADY_EPOCHS * width;
        if steady_metrics.durable_epochs != STEADY_EPOCHS as u64
            || steady_metrics.applied_epochs != STEADY_EPOCHS as u64
            || steady_metrics.published_worlds != STEADY_EPOCHS as u64
            || steady_metrics.accepted_intents != steady_intents as u64
        {
            return Err(format!("{label} steady-state accounting diverged").into());
        }

        let world = controller.snapshot();
        let expected_version = (window + STEADY_EPOCHS) as u64;
        let expected_slots = (window + STEADY_EPOCHS) * width;
        if world.version() != expected_version || world.active_slot_count() != expected_slots {
            return Err(
                format!("{label} final world does not contain every admitted intent").into(),
            );
        }
        let report = CaseReport {
            label: label.to_owned(),
            max_unapplied_epochs: window,
            intents_per_epoch: width,
            journal_ceiling,
            steady_state: SteadyState {
                epochs: STEADY_EPOCHS,
                intents: steady_intents,
                admission_elapsed_us: admission_elapsed.as_micros(),
                publication_elapsed_us: publication_elapsed.as_micros(),
                catch_up_elapsed_us: catch_up_elapsed.as_micros(),
                admission_intents_per_second: rate(steady_intents, admission_elapsed),
                publication_intents_per_second: rate(steady_intents, publication_elapsed),
                lag_when_admission_completed,
                maximum_durable_to_applied_lag: final_metrics.maximum_semantic_lag,
                durable_epochs: steady_metrics.durable_epochs,
                applied_epochs: steady_metrics.applied_epochs,
                published_worlds: steady_metrics.published_worlds,
                data_syncs: steady_metrics.data_syncs,
                syncs_per_intent: steady_metrics.data_syncs as f64 / steady_intents as f64,
                backpressured_intents: steady_metrics.backpressured_intents,
            },
            final_world_version: world.version(),
            final_active_slots: world.active_slot_count(),
            journal_bytes: fs::metadata(path)?.len(),
        };
        controller.shutdown();
        drop(controller);
        Ok(report)
    }

    fn submit_epochs(
        controller: &AdmissionEpochController,
        epoch_offset: usize,
        epoch_count: usize,
        width: usize,
    ) -> Result<Vec<Vec<AdmissionEpochTicket>>, Box<dyn Error>> {
        (0..epoch_count)
            .map(|epoch| {
                let intents = (0..width)
                    .map(|position| {
                        let sequence = (epoch_offset + epoch) * width + position;
                        let mut intent = QueuedIntent::new();
                        intent.define(
                            SlotId::new(format!("benchmark/{sequence}")),
                            IntentFact::new(
                                Literal::new(sequence.to_string()),
                                Predicate::new("benchmark_state"),
                                Literal::new("active"),
                            ),
                        );
                        intent
                    })
                    .collect();
                submit_epoch(controller, intents)
            })
            .collect()
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

    fn wait_admitted(tickets: &[Vec<AdmissionEpochTicket>]) -> Result<(), Box<dyn Error>> {
        for epoch in tickets {
            for ticket in epoch {
                ticket.wait_admitted()?;
            }
        }
        Ok(())
    }

    fn wait_outcomes(tickets: Vec<Vec<AdmissionEpochTicket>>) -> Result<(), Box<dyn Error>> {
        for epoch in tickets {
            for ticket in epoch {
                match ticket.wait()? {
                    AdmissionEpochTicketOutcome::Accepted { .. } => {}
                    outcome => {
                        return Err(format!("unexpected benchmark outcome: {outcome:?}").into());
                    }
                }
            }
        }
        Ok(())
    }

    fn rate(intents: usize, elapsed: Duration) -> f64 {
        intents as f64 / elapsed.as_secs_f64()
    }

    fn delta(after: AdmissionEpochMetrics, before: AdmissionEpochMetrics) -> AdmissionEpochMetrics {
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
            mmap_snapshot_loaded: after.mmap_snapshot_loaded,
            mmap_snapshot_epochs_skipped: after.mmap_snapshot_epochs_skipped,
            mmap_snapshot_bytes: after.mmap_snapshot_bytes,
        }
    }

    fn unavailable(error: &(dyn Error + 'static)) -> bool {
        error
            .downcast_ref::<AdmissionEpochOpenError>()
            .and_then(|error| match error {
                AdmissionEpochOpenError::Io(error) => Some(error),
                _ => None,
            })
            .is_some_and(|error| matches!(error.raw_os_error(), Some(1 | 38 | 95)))
    }
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the admission-window benchmark requires Linux io_uring");
    std::process::exit(2);
}
