use forthdb_core::{Atom, EntityId, Fact, Literal, Predicate, SlotId};
use forthdb_world::{
    BatchPolicy, Database, DurableQueuedIntentController, FileEpochStore, FileEpochSyncPolicy, QueuedIntent,
};
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

static TEMP_SEQ: AtomicU64 = AtomicU64::new(100);

fn temp_db_path() -> std::path::PathBuf {
    let id = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("forthdb_durable_lib_bench_{}_{}.fdb", std::process::id(), id))
}

fn make_library_intent(copy_id: u64, patron_id: u64, cycle: u64) -> QueuedIntent {
    let mut intent = QueuedIntent::new();
    let slot = SlotId::new(format!("copy_{}/status/current", copy_id));
    let status_fact = Fact::new(
        Atom::Entity(EntityId::new(copy_id)),
        Predicate::new("status"),
        Atom::Literal(Literal::new(format!("checked_out_to_{}", patron_id))),
    );
    intent.define_fact(slot, status_fact);

    let log_slot = SlotId::new(format!("copy_{}/event/{}", copy_id, cycle));
    let log_fact = Fact::new(
        Atom::Entity(EntityId::new(copy_id)),
        Predicate::new("event"),
        Atom::Literal(Literal::new("checkout")),
    );
    intent.define_fact(log_slot, log_fact);
    intent
}

fn run_durable_library_benchmark(
    name: &str,
    policy: BatchPolicy,
    concurrent_clients: usize,
    intents_per_client: usize,
) {
    let db_path = temp_db_path();
    let _ = fs::remove_file(&db_path);

    let controller = Arc::new(
        DurableQueuedIntentController::open_owned_with_policy(
            &db_path,
            FileEpochSyncPolicy::PerEpoch,
            65536,
            policy,
        )
        .expect("durable controller starts"),
    );

    let start = Instant::now();
    let mut handles = Vec::new();
    let latencies_mutex = Arc::new(Mutex::new(Vec::with_capacity(concurrent_clients * intents_per_client)));

    for client_idx in 0..concurrent_clients {
        let controller = controller.clone();
        let latencies = latencies_mutex.clone();
        handles.push(thread::spawn(move || {
            let mut client_latencies = Vec::with_capacity(intents_per_client);
            for i in 0..intents_per_client {
                let copy_id = ((client_idx * intents_per_client + i) % 50000) as u64 + 1;
                let patron_id = (client_idx % 1000) as u64 + 1;
                let intent = make_library_intent(copy_id, patron_id, i as u64);
                let t0 = Instant::now();
                let ticket = controller.submit(intent).expect("submit");
                ticket.wait().expect("wait");
                client_latencies.push(t0.elapsed());
            }
            latencies.lock().unwrap().extend(client_latencies);
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let elapsed = start.elapsed();
    let total_intents = concurrent_clients * intents_per_client;
    let ops_per_sec = (total_intents as f64) / elapsed.as_secs_f64();

    let metrics = controller.metrics();
    controller.flush().expect("flush");

    // Extract latency quantiles
    let mut latencies = latencies_mutex.lock().unwrap().clone();
    latencies.sort();
    let p50 = latencies[latencies.len() * 50 / 100];
    let p95 = latencies[latencies.len() * 95 / 100];
    let p99 = latencies[latencies.len() * 99 / 100];

    println!("=== LIBRARY WORKLOAD BENCHMARK: {} ===", name);
    println!("Policy: {:?}", policy);
    println!("Clients: {}, Intents/Client: {}, Total Intents: {}", concurrent_clients, intents_per_client, total_intents);
    println!("Time: {:?}, Ops/sec: {:.2}", elapsed, ops_per_sec);
    println!("Latency p50: {:?}, p95: {:?}, p99: {:?}", p50, p95, p99);
    println!("Epochs/Syncs: {}, Avg Batch Size: {:.2}", metrics.epochs, total_intents as f64 / metrics.epochs as f64);
    println!("Seals: Capacity: {}, Timeout: {}, Drain: {}, Width: {}, Latency: {}, LowTraffic: {}, SourceStalled: {}", 
             metrics.batches_sealed_by_capacity,
             metrics.batches_sealed_by_timeout,
             metrics.batches_sealed_by_drain,
             metrics.batches_sealed_by_width,
             metrics.batches_sealed_by_latency,
             metrics.batches_sealed_by_low_traffic,
             metrics.batches_sealed_by_source_stalled);

    // Verify exact recovery
    let live_version = controller.database().snapshot().version();
    drop(controller);
    let recovered_store = FileEpochStore::open(&db_path, FileEpochSyncPolicy::PerEpoch).expect("reopen store");
    let recovered_db = Database::new(recovered_store).expect("recovered db");
    assert_eq!(recovered_db.snapshot().version(), live_version, "Recovery parity check failed");
    println!("Recovery Parity: OK (Version {})", live_version);
    let _ = fs::remove_file(&db_path);
    println!();
}

fn run_in_memory_library_benchmark(
    name: &str,
    policy: BatchPolicy,
    concurrent_clients: usize,
    intents_per_client: usize,
) {
    let database = Arc::new(
        Database::new(forthdb_world::MemoryCommitStore::new()).expect("empty memory store"),
    );

    let controller = Arc::new(
        forthdb_world::QueuedIntentController::new(
            database.clone(),
            65536,
            policy,
        )
        .expect("in-memory controller starts"),
    );

    let start = Instant::now();
    let mut handles = Vec::new();

    for client_idx in 0..concurrent_clients {
        let controller = controller.clone();
        handles.push(thread::spawn(move || {
            for i in 0..intents_per_client {
                let copy_id = ((client_idx * intents_per_client + i) % 50000) as u64 + 1;
                let patron_id = (client_idx % 1000) as u64 + 1;
                let intent = make_library_intent(copy_id, patron_id, i as u64);
                let ticket = controller.submit(intent).expect("submit");
                ticket.wait().expect("wait");
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let elapsed = start.elapsed();
    let total_intents = concurrent_clients * intents_per_client;
    let ops_per_sec = (total_intents as f64) / elapsed.as_secs_f64();
    let metrics = controller.metrics();

    println!("=== IN-MEMORY LIBRARY BENCHMARK: {} ===", name);
    println!("Policy: {:?}", policy);
    println!("Clients: {}, Total Intents: {}", concurrent_clients, total_intents);
    println!("Time: {:?}, Ops/sec: {:.2}", elapsed, ops_per_sec);
    println!("Epochs: {}, Avg Batch Size: {:.2}", metrics.epochs, total_intents as f64 / metrics.epochs as f64);
    println!();
}

#[cfg(target_os = "linux")]
fn run_io_uring_library_benchmark(
    name: &str,
    policy: BatchPolicy,
    concurrent_clients: usize,
    intents_per_client: usize,
) {
    let db_path = temp_db_path();
    let _ = fs::remove_file(&db_path);

    let store = match forthdb_world::IoUringEpochFileIo::open_store(&db_path) {
        Ok(store) => store,
        Err(err) => {
            println!("=== IO_URING BENCHMARK: {} (UNSUPPORTED IN CONTAINER KERNEL: {}) ===", name, err);
            println!();
            let _ = fs::remove_file(&db_path);
            return;
        }
    };
    let database = Arc::new(Database::new(store).expect("database"));

    let controller = Arc::new(
        DurableQueuedIntentController::new_with_policy(
            database,
            65536,
            policy,
        )
        .expect("durable controller starts"),
    );

    let start = Instant::now();
    let mut handles = Vec::new();

    for client_idx in 0..concurrent_clients {
        let controller = controller.clone();
        handles.push(thread::spawn(move || {
            for i in 0..intents_per_client {
                let copy_id = ((client_idx * intents_per_client + i) % 50000) as u64 + 1;
                let patron_id = (client_idx % 1000) as u64 + 1;
                let intent = make_library_intent(copy_id, patron_id, i as u64);
                let ticket = controller.submit(intent).expect("submit");
                ticket.wait().expect("wait");
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let elapsed = start.elapsed();
    let total_intents = concurrent_clients * intents_per_client;
    let ops_per_sec = (total_intents as f64) / elapsed.as_secs_f64();
    let metrics = controller.metrics();
    controller.flush().expect("flush");

    println!("=== IO_URING BENCHMARK: {} ===", name);
    println!("Policy: {:?}", policy);
    println!("Clients: {}, Total Intents: {}", concurrent_clients, total_intents);
    println!("Time: {:?}, Ops/sec: {:.2}", elapsed, ops_per_sec);
    println!("Epochs: {}, Avg Batch Size: {:.2}", metrics.epochs, total_intents as f64 / metrics.epochs as f64);
    println!();
    let _ = fs::remove_file(&db_path);
}

#[test]
#[ignore]
fn run_library_workload_experiment() {
    println!("===========================================================");
    println!("     STARTING REAL LIBRARY CIRCULATION WORKLOAD TEST       ");
    println!("===========================================================");

    for clients in [16, 64, 100] {
        let intents = 100;

        println!(">>> LIBRARY WORKLOAD (C={}) <<<", clients);
        run_durable_library_benchmark(
            &format!("ImmediateDrain (Durable C={})", clients),
            BatchPolicy::ImmediateDrain { max_batch: 4096 },
            clients,
            intents,
        );
        run_durable_library_benchmark(
            &format!("Adaptive (Durable C={})", clients),
            BatchPolicy::Adaptive { min_batch: 1, max_batch: 4096, latency_budget: Duration::from_millis(2) },
            clients,
            intents,
        );

        #[cfg(target_os = "linux")]
        {
            run_io_uring_library_benchmark(
                &format!("ImmediateDrain (io_uring C={})", clients),
                BatchPolicy::ImmediateDrain { max_batch: 4096 },
                clients,
                intents,
            );
            run_io_uring_library_benchmark(
                &format!("Adaptive (io_uring C={})", clients),
                BatchPolicy::Adaptive { min_batch: 1, max_batch: 4096, latency_budget: Duration::from_millis(2) },
                clients,
                intents,
            );
        }

        run_in_memory_library_benchmark(
            &format!("ImmediateDrain (In-Mem C={})", clients),
            BatchPolicy::ImmediateDrain { max_batch: 4096 },
            clients,
            intents,
        );
        run_in_memory_library_benchmark(
            &format!("Adaptive (In-Mem C={})", clients),
            BatchPolicy::Adaptive { min_batch: 1, max_batch: 4096, latency_budget: Duration::from_millis(2) },
            clients,
            intents,
        );
    }
}
