use forthdb_world::{
    BatchPolicy, Database, MemoryCommitStore, QueuedIntent, QueuedIntentController,
};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn run_benchmark(
    name: &str,
    policy: BatchPolicy,
    concurrent_clients: usize,
    intents_per_client: usize,
    client_delay: Duration,
) {
    let database = Arc::new(
        Database::new(MemoryCommitStore::new()).expect("empty memory store is valid"),
    );
    let controller = Arc::new(
        QueuedIntentController::new(database.clone(), 65536, policy)
            .expect("controller starts"),
    );

    let start = Instant::now();
    let mut handles = Vec::new();

    for _ in 0..concurrent_clients {
        let controller = controller.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..intents_per_client {
                let intent = QueuedIntent::new();
                let ticket = controller.submit(intent).expect("submit");
                ticket.wait().expect("wait");
                if client_delay > Duration::ZERO {
                    thread::sleep(client_delay);
                }
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
    
    println!("--- Benchmark: {} ---", name);
    println!("Policy: {:?}", policy);
    println!("Clients: {}, Intents/Client: {}, Client Delay: {:?}", concurrent_clients, intents_per_client, client_delay);
    println!("Time: {:?}, Ops/sec: {:.2}", elapsed, ops_per_sec);
    println!("Epochs: {}, Avg Batch Size: {:.2}", metrics.epochs, total_intents as f64 / metrics.epochs as f64);
    println!("Batches Sealed By: Capacity: {}, Timeout: {}, Drain: {}, Width: {}, Latency: {}, LowTraffic: {}, SourceStalled: {}, Barrier: {}", 
             metrics.batches_sealed_by_capacity,
             metrics.batches_sealed_by_timeout,
             metrics.batches_sealed_by_drain,
             metrics.batches_sealed_by_width,
             metrics.batches_sealed_by_latency,
             metrics.batches_sealed_by_low_traffic,
             metrics.batches_sealed_by_source_stalled,
             metrics.batches_sealed_by_barrier);
    if let BatchPolicy::Adaptive { .. } = policy {
        println!("  Max Target Width: {}, Total Probe Wait: {:?}, Max Oldest Age at Seal: {:?}", 
                 metrics.maximum_target_width, 
                 Duration::from_nanos(metrics.total_adaptive_probe_wait_ns),
                 Duration::from_nanos(metrics.maximum_oldest_age_at_seal_ns));
    }
    println!();
}

#[test]
#[ignore]
fn run_controlled_arrival_rate_benchmarks() {
    println!("Starting Controlled Arrival-Rate Benchmarks");

    // Saturated load (no delay between intents)
    run_benchmark(
        "ImmediateDrain - Saturated",
        BatchPolicy::ImmediateDrain { max_batch: 4096 },
        100,
        1000,
        Duration::ZERO,
    );
    run_benchmark(
        "Coalesce - Saturated",
        BatchPolicy::Coalesce { max_batch: 4096, max_delay: Duration::from_millis(2) },
        100,
        1000,
        Duration::ZERO,
    );
    run_benchmark(
        "Adaptive - Saturated",
        BatchPolicy::Adaptive { min_batch: 1, max_batch: 4096, latency_budget: Duration::from_millis(2) },
        100,
        1000,
        Duration::ZERO,
    );

    // Mid load (small delay between intents)
    run_benchmark(
        "ImmediateDrain - Mid Load",
        BatchPolicy::ImmediateDrain { max_batch: 4096 },
        100,
        100,
        Duration::from_micros(500),
    );
    run_benchmark(
        "Coalesce - Mid Load",
        BatchPolicy::Coalesce { max_batch: 4096, max_delay: Duration::from_millis(2) },
        100,
        100,
        Duration::from_micros(500),
    );
    run_benchmark(
        "Adaptive - Mid Load",
        BatchPolicy::Adaptive { min_batch: 1, max_batch: 4096, latency_budget: Duration::from_millis(2) },
        100,
        100,
        Duration::from_micros(500),
    );

    // Low load (large delay between intents)
    run_benchmark(
        "ImmediateDrain - Low Load",
        BatchPolicy::ImmediateDrain { max_batch: 4096 },
        10,
        100,
        Duration::from_millis(5),
    );
    run_benchmark(
        "Coalesce - Low Load",
        BatchPolicy::Coalesce { max_batch: 4096, max_delay: Duration::from_millis(2) },
        10,
        100,
        Duration::from_millis(5),
    );
    run_benchmark(
        "Adaptive - Low Load",
        BatchPolicy::Adaptive { min_batch: 1, max_batch: 4096, latency_budget: Duration::from_millis(2) },
        10,
        100,
        Duration::from_millis(5),
    );
}
