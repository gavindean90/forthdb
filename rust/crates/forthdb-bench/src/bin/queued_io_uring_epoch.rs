#[cfg(target_os = "linux")]
mod linux {
    use forthdb_core::{Atom, EntityId, Fact, ForthDb, Literal, Predicate, SlotId};
    use forthdb_world::{
        CommitStore, Database, DurableQueuedIntentController, DurableSubmitError, DurableTicketOutcome,
        EpochFileIo, FileCommitStore, FileEpochMetrics, FileEpochStore, FileEpochStoreError,
        FileEpochSyncPolicy, IoUringEpochFileIo, IoUringEpochStrategy, QueuedIntent,
    };
    use serde::Serialize;
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{mpsc, Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};

    const RETAINED_DEFINITIONS: usize = 100_000;
    const CAPACITY: usize = 256;
    const PRODUCERS: usize = 4;
    const ROUNDS: usize = 4;
    const RING_ENTRIES: u32 = 64;
    const RETRY_PAUSE: Duration = Duration::from_micros(10);
    static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Variant {
        Ordinary,
        RingContiguous,
        RingVectored,
        RingPipelined,
    }

    impl Variant {
        const ALL: [Self; 4] = [
            Self::Ordinary,
            Self::RingContiguous,
            Self::RingVectored,
            Self::RingPipelined,
        ];

        const fn label(self) -> &'static str {
            match self {
                Self::Ordinary => "ordinary_per_epoch",
                Self::RingContiguous => "io_uring_contiguous_write",
                Self::RingVectored => "io_uring_writev",
                Self::RingPipelined => "io_uring_pipelined_writes",
            }
        }
    }

    #[derive(Debug, Serialize)]
    struct Report {
        status: &'static str,
        scope: &'static str,
        retained_definitions: usize,
        capacity: usize,
        producers: usize,
        rounds: usize,
        ring_entries: u32,
        environment: Environment,
        configurations: Vec<Configuration>,
        availability_error: Option<String>,
        total_elapsed_ms: u128,
    }

    #[derive(Debug, Serialize)]
    struct Environment {
        os: &'static str,
        architecture: &'static str,
        profile: &'static str,
        git_sha: Option<String>,
        github_run_id: Option<String>,
    }

    #[derive(Debug, Serialize)]
    struct Configuration {
        variant: &'static str,
        max_batch: usize,
        intents_per_round: usize,
        samples: usize,
        median_intents_per_second: f64,
        min_intents_per_second: f64,
        max_intents_per_second: f64,
        median_ns_per_intent: f64,
        median_epochs: u64,
        median_batch: f64,
        median_data_writes: u64,
        median_data_syncs: u64,
        median_submission_calls: u64,
        median_completion_events: u64,
        maximum_in_flight_writes: u64,
        median_iovecs_submitted: u64,
        median_arena_bytes_copied: u64,
        median_bytes_written: u64,
        median_backpressure_events: u64,
        sample_throughput: Vec<f64>,
    }

    #[derive(Debug)]
    struct Sample {
        elapsed_ns: u128,
        intents_per_second: f64,
        epochs: u64,
        average_batch: f64,
        store: FileEpochMetrics,
        backpressure_events: u64,
    }

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "forthdb-m6c-{label}-{}-{sequence}.db",
                std::process::id()
            ));
            let _ = fs::remove_file(&path);
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    pub fn main() {
        let total_started = Instant::now();
        let probe = TempFile::new("probe");
        let availability_error = match IoUringEpochFileIo::open_store_with_entries(
            probe.path(),
            IoUringEpochStrategy::ContiguousWrite,
            RING_ENTRIES,
        ) {
            Ok(_) => None,
            Err(error) if unavailable(&error) => Some(error.to_string()),
            Err(error) => panic!("io_uring epoch probe failed unexpectedly: {error}"),
        };

        let configurations = if availability_error.is_some() {
            Vec::new()
        } else {
            let base = build_base_file();
            let mut grouped = Vec::new();
            for (max_batch, intents_per_producer) in [(1usize, 128usize), (16, 512)] {
                let mut samples = Variant::ALL
                    .into_iter()
                    .map(|variant| (variant, Vec::with_capacity(ROUNDS)))
                    .collect::<Vec<_>>();

                // Rotate the starting position so every variant occupies every
                // filesystem/warm-up position exactly once.
                for round in 0..ROUNDS {
                    for offset in 0..Variant::ALL.len() {
                        let variant = Variant::ALL[(round + offset) % Variant::ALL.len()];
                        let sample = run_sample(
                            base.path(),
                            variant,
                            max_batch,
                            intents_per_producer,
                        );
                        samples
                            .iter_mut()
                            .find(|(candidate, _)| *candidate == variant)
                            .expect("variant sample group exists")
                            .1
                            .push(sample);
                    }
                }

                for (variant, variant_samples) in samples {
                    grouped.push(summarize(
                        variant,
                        max_batch,
                        PRODUCERS * intents_per_producer,
                        variant_samples,
                    ));
                }
            }
            grouped
        };

        let report = Report {
            status: if availability_error.is_some() {
                "unavailable"
            } else {
                "observational"
            },
            scope: "io-uring-durability-epochs",
            retained_definitions: RETAINED_DEFINITIONS,
            capacity: CAPACITY,
            producers: PRODUCERS,
            rounds: ROUNDS,
            ring_entries: RING_ENTRIES,
            environment: Environment {
                os: env::consts::OS,
                architecture: env::consts::ARCH,
                profile: if cfg!(debug_assertions) { "debug" } else { "release" },
                git_sha: env::var("GITHUB_SHA").ok(),
                github_run_id: env::var("GITHUB_RUN_ID").ok(),
            },
            configurations,
            availability_error,
            total_elapsed_ms: total_started.elapsed().as_millis(),
        };
        let json = serde_json::to_string_pretty(&report).expect("report serializes");
        if let Ok(path) = env::var("FORTHDB_IO_URING_EPOCH_REPORT") {
            fs::write(path, format!("{json}\n")).expect("report writes");
        } else {
            println!("{json}");
        }
    }

    fn unavailable(error: &FileEpochStoreError) -> bool {
        match error {
            FileEpochStoreError::Io { source, .. } => {
                matches!(source.raw_os_error(), Some(1 | 38 | 95))
            }
            _ => false,
        }
    }

    fn build_base_file() -> TempFile {
        let base = TempFile::new("base");
        let database = Database::new(
            FileCommitStore::open(base.path()).expect("base file store opens"),
        )
        .expect("base file database reconstructs");
        let mut transaction = database.begin();
        assert_eq!(transaction.entity(), EntityId::new(1));
        for index in 0..RETAINED_DEFINITIONS {
            transaction.define(
                SlotId::new(format!("base/{index}")),
                Fact::new(
                    Atom::Entity(EntityId::new(1)),
                    Predicate::new("base_state"),
                    Atom::Literal(Literal::new(index.to_string())),
                ),
            );
        }
        database.commit(transaction).expect("base world commits");
        drop(database);
        base
    }

    fn queued_fact(sequence: usize) -> Fact {
        Fact::new(
            Atom::Entity(EntityId::new(1)),
            Predicate::new("queued_state"),
            Atom::Literal(Literal::new(sequence.to_string())),
        )
    }

    fn run_sample(
        base_path: &Path,
        variant: Variant,
        max_batch: usize,
        intents_per_producer: usize,
    ) -> Sample {
        assert!(ForthDb::drain_reaper(Duration::from_secs(30)));
        let temp = TempFile::new(variant.label());
        fs::copy(base_path, temp.path()).expect("base file copies");
        match variant {
            Variant::Ordinary => {
                let store = FileEpochStore::open(temp.path(), FileEpochSyncPolicy::PerEpoch)
                    .expect("ordinary epoch store opens");
                run_controller(temp, store, max_batch, intents_per_producer)
            }
            Variant::RingContiguous => {
                let store = IoUringEpochFileIo::open_store_with_entries(
                    temp.path(),
                    IoUringEpochStrategy::ContiguousWrite,
                    RING_ENTRIES,
                )
                .expect("contiguous ring epoch store opens");
                run_controller(temp, store, max_batch, intents_per_producer)
            }
            Variant::RingVectored => {
                let store = IoUringEpochFileIo::open_store_with_entries(
                    temp.path(),
                    IoUringEpochStrategy::VectoredWrite,
                    RING_ENTRIES,
                )
                .expect("vectored ring epoch store opens");
                run_controller(temp, store, max_batch, intents_per_producer)
            }
            Variant::RingPipelined => {
                let store = IoUringEpochFileIo::open_store_with_entries(
                    temp.path(),
                    IoUringEpochStrategy::PipelinedWrites,
                    RING_ENTRIES,
                )
                .expect("pipelined ring epoch store opens");
                run_controller(temp, store, max_batch, intents_per_producer)
            }
        }
    }

    fn run_controller<I: EpochFileIo + 'static>(
        temp: TempFile,
        store: FileEpochStore<I>,
        max_batch: usize,
        intents_per_producer: usize,
    ) -> Sample {
        let database = Arc::new(Database::new(store).expect("epoch database reconstructs"));
        let base_version = database.snapshot().version();
        let base_frame_count = database.frame_count();
        let controller = Arc::new(
            DurableQueuedIntentController::new(database.clone(), CAPACITY, max_batch)
                .expect("durable controller starts"),
        );
        let total = PRODUCERS * intents_per_producer;
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
        assert_eq!(final_frame_count, base_frame_count + total);
        assert_eq!(store_metrics.frames_committed, total as u64);
        assert_eq!(store_metrics.data_syncs, controller_metrics.epochs);
        if max_batch == 1 {
            assert_eq!(controller_metrics.epochs, total as u64);
        }

        drop(controller);
        drop(database);
        let recovered = FileCommitStore::open(temp.path()).expect("epoch file reopens");
        assert_eq!(recovered.len(), final_frame_count);
        drop(recovered);
        assert!(ForthDb::drain_reaper(Duration::from_secs(30)));

        let elapsed_ns = elapsed.as_nanos();
        Sample {
            elapsed_ns,
            intents_per_second: total as f64 / elapsed.as_secs_f64(),
            epochs: controller_metrics.epochs,
            average_batch: total as f64 / controller_metrics.epochs as f64,
            store: store_metrics,
            backpressure_events: controller_metrics.backpressured,
        }
    }

    fn summarize(
        variant: Variant,
        max_batch: usize,
        intents_per_round: usize,
        mut samples: Vec<Sample>,
    ) -> Configuration {
        samples.sort_by(|left, right| left.elapsed_ns.cmp(&right.elapsed_ns));
        let median = &samples[samples.len() / 2];
        let mut throughputs = samples
            .iter()
            .map(|sample| sample.intents_per_second)
            .collect::<Vec<_>>();
        throughputs.sort_by(|left, right| left.total_cmp(right));

        Configuration {
            variant: variant.label(),
            max_batch,
            intents_per_round,
            samples: samples.len(),
            median_intents_per_second: median.intents_per_second,
            min_intents_per_second: throughputs[0],
            max_intents_per_second: throughputs[throughputs.len() - 1],
            median_ns_per_intent: median.elapsed_ns as f64 / intents_per_round as f64,
            median_epochs: median.epochs,
            median_batch: median.average_batch,
            median_data_writes: median.store.data_writes,
            median_data_syncs: median.store.data_syncs,
            median_submission_calls: median.store.submission_calls,
            median_completion_events: median.store.completion_events,
            maximum_in_flight_writes: samples
                .iter()
                .map(|sample| sample.store.maximum_in_flight_writes)
                .max()
                .unwrap_or(0),
            median_iovecs_submitted: median.store.iovecs_submitted,
            median_arena_bytes_copied: median.store.arena_bytes_copied,
            median_bytes_written: median.store.bytes_written,
            median_backpressure_events: median.backpressure_events,
            sample_throughput: samples
                .iter()
                .map(|sample| sample.intents_per_second)
                .collect(),
        }
    }
}

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!(
        "{{\"status\":\"unavailable\",\"scope\":\"io-uring-durability-epochs\",\"availability_error\":\"Linux only\"}}"
    );
}
