use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use forthdb_core::{Literal, Predicate, SlotId};
use forthdb_world::transaction_ast::{
    AtomRef, EntityId as AstEntityId, SemanticBindings, SemanticIntent, TransactionAST,
    TransactionOp,
};
use forthdb_world::{
    BatchPolicy, CandidateWorld, ControllerIntent, Database, IntentAtom, IntentFact,
    MemoryCommitStore, QueuedIntentController, SemanticTicketOutcome, TicketOutcome, World,
};
use forthdb_world::{QueuedIntent, VmEpochMaterializer};

struct TrackingAllocator;

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static TRACKING_ENABLED: AtomicBool = AtomicBool::new(false);

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() && TRACKING_ENABLED.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
    }
}

#[global_allocator]
static GLOBAL: TrackingAllocator = TrackingAllocator;

fn reset_alloc() {
    ALLOC_COUNT.store(0, Ordering::Relaxed);
    ALLOC_BYTES.store(0, Ordering::Relaxed);
}

fn start_alloc() {
    reset_alloc();
    TRACKING_ENABLED.store(true, Ordering::Relaxed);
}

fn stop_alloc() -> (u64, u64) {
    TRACKING_ENABLED.store(false, Ordering::Relaxed);
    (
        ALLOC_COUNT.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
    )
}

#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug)]
enum BenchmarkControl {
    A_QueuedLegacy,
    B_QueuedMixed,
    C_SemanticWarm,
    D_SemanticNew,
    E_MixedFiftyFifty,
    F_SemanticAcceptingValidator,
    G_SemanticRejectingValidator,
}

impl BenchmarkControl {
    fn label(&self) -> &'static str {
        match self {
            Self::A_QueuedLegacy => "A. QueuedIntent Legacy",
            Self::B_QueuedMixed => "B. QueuedIntent Mixed",
            Self::C_SemanticWarm => "C. SemanticIntent Warm",
            Self::D_SemanticNew => "D. SemanticIntent New",
            Self::E_MixedFiftyFifty => "E. 50/50 Queued+Semantic",
            Self::F_SemanticAcceptingValidator => "F. Semantic Accepting Validator",
            Self::G_SemanticRejectingValidator => "G. Semantic Rejecting Validator",
        }
    }
}

fn populate_database(size: usize) -> (Arc<Database<MemoryCommitStore>>, Arc<World>) {
    let database = Arc::new(Database::new(MemoryCommitStore::new()).unwrap());
    if size == 0 {
        return (database.clone(), database.snapshot());
    }

    let mut materializer = VmEpochMaterializer::new(1);
    let batch_size = 10000;
    let mut current_base = database.snapshot();

    for batch_start in (0..size).step_by(batch_size) {
        let chunk_end = (batch_start + batch_size).min(size);
        let mut intents = Vec::with_capacity(chunk_end - batch_start);
        for i in batch_start..chunk_end {
            let mut q = QueuedIntent::new();
            let ent = q.entity();
            q.define(
                SlotId::new(&format!("init_slot_{i}")),
                IntentFact {
                    subject: IntentAtom::Temporary(ent),
                    predicate: Predicate::new("init_pred"),
                    object: IntentAtom::Literal(Literal::new(&format!("init_val_{i}"))),
                },
            );
            intents.push(ControllerIntent::Queued(q));
        }
        let plan = database.commit_mixed_epoch(intents, &mut materializer);
        current_base = plan.tail();
    }

    (database, current_base)
}

fn main() {
    println!("==================================================================================");
    println!("             FORTHDB PHASE 2B NON-DURABLE SEMANTIC INGRESS BENCHMARK              ");
    println!("==================================================================================");

    // 1. Pipeline & Worker Capacities
    run_microkernel_capacity_benchmarks();

    // 2. Allocation Instrumentation (7 Stages)
    run_stage_allocation_benchmarks();

    // 3. Full Matrix Benchmark (Controls A-G across state sizes, batch policies, concurrency)
    run_full_controller_benchmarks();

    println!("\nBenchmark run completed cleanly!");
}

fn run_microkernel_capacity_benchmarks() {
    println!("\n--- Stage 0: Worker Transient Trial & Materializer Pipeline Capacity ---");

    let iterations = 100_000usize;
    let database = Database::new(MemoryCommitStore::new()).unwrap();
    let mut materializer = VmEpochMaterializer::new(1);
    let base = database.snapshot();

    let warm_ast = TransactionAST::new(
        1,
        vec![
            TransactionOp::Allocate {
                result: "book".to_owned(),
            },
            TransactionOp::Define {
                slot: "book/status".to_owned(),
                subject: AtomRef::Symbol("book".to_owned()),
                predicate: "is".to_owned(),
                object: AtomRef::Literal("available".to_owned()),
            },
        ],
    );

    // Warm up vocabulary
    let _ = materializer.materialize_mixed(
        base.clone(),
        vec![ControllerIntent::Semantic(SemanticIntent::new(warm_ast))],
        &[],
    );

    let start = Instant::now();
    for i in 0..iterations {
        let ast = TransactionAST::new(
            (i + 2) as u64,
            vec![
                TransactionOp::Allocate {
                    result: "book".to_owned(),
                },
                TransactionOp::Define {
                    slot: "book/status".to_owned(),
                    subject: AtomRef::Symbol("book".to_owned()),
                    predicate: "is".to_owned(),
                    object: AtomRef::Literal("available".to_owned()),
                },
            ],
        );
        let plan = materializer.materialize_mixed(
            base.clone(),
            vec![ControllerIntent::Semantic(SemanticIntent::new(ast))],
            &[],
        );
        std::hint::black_box(plan);
    }
    let elapsed = start.elapsed();
    let worker_tps = (iterations as f64) / elapsed.as_secs_f64();
    let worker_ns = elapsed.as_nanos() as f64 / iterations as f64;

    println!(
        "Worker Transient Trial Capacity:        {:>8.2} ns/op  |  {:>10.2} M TPS",
        worker_ns,
        worker_tps / 1e6
    );
}

fn run_stage_allocation_benchmarks() {
    println!("\n--- Stage Allocations & Memory Footprint Analysis (7 Stages) ---");

    // Stage 1: Producer SemanticIntent Construction
    start_alloc();
    for i in 0..10_000 {
        let ast = TransactionAST::new(
            i,
            vec![
                TransactionOp::Allocate {
                    result: "ent".to_owned(),
                },
                TransactionOp::Define {
                    slot: "bench/slot".to_owned(),
                    subject: AtomRef::Symbol("ent".to_owned()),
                    predicate: "pred".to_owned(),
                    object: AtomRef::Literal("val".to_owned()),
                },
            ],
        );
        let intent = SemanticIntent::new(ast);
        std::hint::black_box(intent);
    }
    let (c1, b1) = stop_alloc();

    // Stage 2: Queue / Ticket Admission
    let database = Arc::new(Database::new(MemoryCommitStore::new()).unwrap());
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

    start_alloc();
    let mut tickets = Vec::with_capacity(10_000);
    for i in 0..10_000 {
        let ast = TransactionAST::new(
            i,
            vec![TransactionOp::Allocate {
                result: "e".to_owned(),
            }],
        );
        let ticket = controller
            .submit_semantic(SemanticIntent::new(ast))
            .unwrap();
        tickets.push(ticket);
    }
    let (c2, b2) = stop_alloc();
    std::hint::black_box(tickets);

    // Stage 3: Warm-vocabulary Semantic Trial
    let mut materializer = VmEpochMaterializer::new(1);
    let base = database.snapshot();
    let warm_ast = TransactionAST::new(
        1,
        vec![
            TransactionOp::Allocate {
                result: "e".to_owned(),
            },
            TransactionOp::Define {
                slot: "warm/slot".to_owned(),
                subject: AtomRef::Symbol("e".to_owned()),
                predicate: "is".to_owned(),
                object: AtomRef::Literal("val".to_owned()),
            },
        ],
    );
    let _ = materializer.materialize_mixed(
        base.clone(),
        vec![ControllerIntent::Semantic(SemanticIntent::new(warm_ast))],
        &[],
    );

    start_alloc();
    for i in 0..10_000 {
        let ast = TransactionAST::new(
            (i + 2) as u64,
            vec![
                TransactionOp::Allocate {
                    result: "e".to_owned(),
                },
                TransactionOp::Define {
                    slot: "warm/slot".to_owned(),
                    subject: AtomRef::Symbol("e".to_owned()),
                    predicate: "is".to_owned(),
                    object: AtomRef::Literal("val".to_owned()),
                },
            ],
        );
        let plan = materializer.materialize_mixed(
            base.clone(),
            vec![ControllerIntent::Semantic(SemanticIntent::new(ast))],
            &[],
        );
        std::hint::black_box(plan);
    }
    let (c3, b3) = stop_alloc();

    // Stage 4: New-vocabulary Semantic Trial
    start_alloc();
    for i in 0..10_000 {
        let ast = TransactionAST::new(
            (i + 20000) as u64,
            vec![
                TransactionOp::Allocate {
                    result: "e".to_owned(),
                },
                TransactionOp::Define {
                    slot: format!("new_slot_{i}"),
                    subject: AtomRef::Symbol("e".to_owned()),
                    predicate: "is".to_owned(),
                    object: AtomRef::Literal(format!("new_val_{i}")),
                },
            ],
        );
        let plan = materializer.materialize_mixed(
            base.clone(),
            vec![ControllerIntent::Semantic(SemanticIntent::new(ast))],
            &[],
        );
        std::hint::black_box(plan);
    }
    let (c4, b4) = stop_alloc();

    // Stage 5: Accepted Publication & Frame Construction
    start_alloc();
    for i in 0..1_000 {
        let ast = TransactionAST::new(
            i,
            vec![TransactionOp::Allocate {
                result: "e".to_owned(),
            }],
        );
        let intent = ControllerIntent::Semantic(SemanticIntent::new(ast));
        let plan = database.commit_mixed_epoch(vec![intent], &mut materializer);
        std::hint::black_box(plan);
    }
    let (c5, b5) = stop_alloc();

    // Stage 6: Validator Path
    database.register_validator(|candidate: &CandidateWorld| {
        candidate
            .resolve(&SlotId::new("warm/slot"))
            .is_some()
            .then_some(())
            .ok_or_else(|| "err".to_owned())
    });

    start_alloc();
    for i in 0..1_000 {
        let ast = TransactionAST::new(
            i + 10000,
            vec![TransactionOp::Allocate {
                result: "e".to_owned(),
            }],
        );
        let intent = ControllerIntent::Semantic(SemanticIntent::new(ast));
        let plan = database.commit_mixed_epoch(vec![intent], &mut materializer);
        std::hint::black_box(plan);
    }
    let (c6, b6) = stop_alloc();

    // Stage 7: Ticket Result / Bindings Construction
    let mut entries = Vec::with_capacity(4);
    start_alloc();
    for i in 0..10_000 {
        entries.clear();
        entries.push(("symbol_a".to_owned(), AstEntityId(i)));
        entries.push(("symbol_b".to_owned(), AstEntityId(i + 1)));
        let bindings = SemanticBindings::new(entries.clone());
        let outcome = SemanticTicketOutcome::Accepted {
            world: base.clone(),
            bindings,
        };
        std::hint::black_box(outcome);
    }
    let (c7, b7) = stop_alloc();

    println!(
        "Stage 1 (Producer Intent Const.):   {:>8} allocs  |  {:>10} bytes ({:.2} allocs/op, {:.2} bytes/op)",
        c1,
        b1,
        c1 as f64 / 10000.0,
        b1 as f64 / 10000.0
    );
    println!(
        "Stage 2 (Queue/Ticket Admission):   {:>8} allocs  |  {:>10} bytes ({:.2} allocs/op, {:.2} bytes/op)",
        c2,
        b2,
        c2 as f64 / 10000.0,
        b2 as f64 / 10000.0
    );
    println!(
        "Stage 3 (Warm Vocabulary Trial):    {:>8} allocs  |  {:>10} bytes ({:.2} allocs/op, {:.2} bytes/op)",
        c3,
        b3,
        c3 as f64 / 10000.0,
        b3 as f64 / 10000.0
    );
    println!(
        "Stage 4 (New Vocabulary Trial):     {:>8} allocs  |  {:>10} bytes ({:.2} allocs/op, {:.2} bytes/op)",
        c4,
        b4,
        c4 as f64 / 10000.0,
        b4 as f64 / 10000.0
    );
    println!(
        "Stage 5 (Accepted Publication):     {:>8} allocs  |  {:>10} bytes ({:.2} allocs/op, {:.2} bytes/op)",
        c5,
        b5,
        c5 as f64 / 1000.0,
        b5 as f64 / 1000.0
    );
    println!(
        "Stage 6 (Validator Path):           {:>8} allocs  |  {:>10} bytes ({:.2} allocs/op, {:.2} bytes/op)",
        c6,
        b6,
        c6 as f64 / 1000.0,
        b6 as f64 / 1000.0
    );
    println!(
        "Stage 7 (Ticket Bindings Result):   {:>8} allocs  |  {:>10} bytes ({:.2} allocs/op, {:.2} bytes/op)",
        c7,
        b7,
        c7 as f64 / 10000.0,
        b7 as f64 / 10000.0
    );
}

fn run_full_controller_benchmarks() {
    println!(
        "\n=================================================================================="
    );
    println!("                FULL CONTROLLER BENCHMARK MATRIX (CONTROLS A-G)                   ");
    println!("==================================================================================");

    run_controller_matrix();
}

fn populate_base_store(size: usize) -> MemoryCommitStore {
    let store = MemoryCommitStore::new();
    if size == 0 {
        return store;
    }
    let database = Arc::new(Database::new(store).unwrap());
    let mut mat = VmEpochMaterializer::new(1);
    let mut intents = Vec::with_capacity(size);
    for i in 0..size {
        let mut q = QueuedIntent::new();
        let ent = q.entity();
        q.define(
            SlotId::new(&format!("init_slot_{i}")),
            IntentFact {
                subject: IntentAtom::Temporary(ent),
                predicate: Predicate::new("init_pred"),
                object: IntentAtom::Literal(Literal::new(&format!("init_val_{i}"))),
            },
        );
        intents.push(ControllerIntent::Queued(q));
    }
    database.commit_mixed_epoch(intents, &mut mat);
    database.store_clone()
}

fn run_controller_matrix() {
    let state_sizes = [
        ("Genesis / Tiny", 0usize),
        ("10,000 Defs", 10_000usize),
        ("100,000 Defs", 100_000usize),
    ];

    let batch_configs = [
        ("Batch W=1", BatchPolicy::ImmediateDrain { max_batch: 1 }),
        ("Batch W=16", BatchPolicy::ImmediateDrain { max_batch: 16 }),
        ("Batch W=64", BatchPolicy::ImmediateDrain { max_batch: 64 }),
        (
            "Adaptive",
            BatchPolicy::Adaptive {
                min_batch: 1,
                max_batch: 64,
                latency_budget: Duration::from_millis(1),
            },
        ),
    ];

    let controls = [
        BenchmarkControl::A_QueuedLegacy,
        BenchmarkControl::B_QueuedMixed,
        BenchmarkControl::C_SemanticWarm,
        BenchmarkControl::D_SemanticNew,
        BenchmarkControl::E_MixedFiftyFifty,
        BenchmarkControl::F_SemanticAcceptingValidator,
        BenchmarkControl::G_SemanticRejectingValidator,
    ];

    let concurrencies = [1, 16, 64];

    for (state_name, state_size) in state_sizes {
        println!("\n>>>>>>>> STATE SIZE: {} <<<<<<<<", state_name);
        let base_store = populate_base_store(state_size);

        for (policy_name, policy) in batch_configs {
            for concurrency in concurrencies {
                println!(
                    "\n--- Config: Policy={}, Concurrency={} ---",
                    policy_name, concurrency
                );

                for control in controls {
                    execute_single_benchmark_run(control, state_size, policy, concurrency, &base_store);
                }
            }
        }
    }
}

fn execute_single_benchmark_run(
    control: BenchmarkControl,
    retained_state_size: usize,
    batch_policy: BatchPolicy,
    procurrency: usize,
    base_store: &MemoryCommitStore,
) {
    let total_intents = match (retained_state_size, matches!(batch_policy, BatchPolicy::ImmediateDrain { max_batch: 1 })) {
        (0, _) => 2_000usize,
        (10_000, _) => 1_000usize,
        (_, true) => 50usize,
        (_, false) => 500usize,
    };
    let intents_per_producer = (total_intents / procurrency).max(1);
    let total_intents = intents_per_producer * procurrency;

    let database = Arc::new(Database::new(base_store.clone()).unwrap());
    let initial_base = database.snapshot();

    if matches!(control, BenchmarkControl::F_SemanticAcceptingValidator) {
        database.register_validator(|_| Ok(()));
    } else if matches!(control, BenchmarkControl::G_SemanticRejectingValidator) {
        database.register_validator(|candidate: &CandidateWorld| {
            if candidate.resolve(&SlotId::new("reject_trigger")).is_some() {
                Err("rejected by host validator".to_owned())
            } else {
                Ok(())
            }
        });
    }

    let controller =
        Arc::new(QueuedIntentController::new(database.clone(), 100_000, batch_policy).unwrap());

    // Warm-up vocabulary for warm semantic control
    if matches!(control, BenchmarkControl::C_SemanticWarm) {
        let warm_ast = TransactionAST::new(
            0,
            vec![TransactionOp::Define {
                slot: "bench_warm_slot".to_owned(),
                subject: AtomRef::Literal("sub".to_owned()),
                predicate: "pred".to_owned(),
                object: AtomRef::Literal("val".to_owned()),
            }],
        );
        let _ = controller.submit_semantic(SemanticIntent::new(warm_ast));
        std::thread::sleep(Duration::from_millis(10));
    }

    let start_time = Instant::now();
    let (submit_tx, submit_rx) = std::sync::mpsc::channel();
    let barrier = Arc::new(std::sync::Barrier::new(procurrency + 1));

    let mut workers = Vec::with_capacity(procurrency);

    for p in 0..procurrency {
        let controller_ref = controller.clone();
        let submit_tx_ref = submit_tx.clone();
        let barrier_ref = barrier.clone();

        workers.push(std::thread::spawn(move || {
            barrier_ref.wait();

            for i in 0..intents_per_producer {
                let idx = p * intents_per_producer + i;
                let submit_start = Instant::now();

                match control {
                    BenchmarkControl::A_QueuedLegacy | BenchmarkControl::B_QueuedMixed => {
                        let mut q = QueuedIntent::new();
                        let ent = q.entity();
                        q.define(
                            SlotId::new("bench_slot"),
                            IntentFact {
                                subject: IntentAtom::Temporary(ent),
                                predicate: Predicate::new("pred"),
                                object: IntentAtom::Literal(Literal::new(&format!("val_{idx}"))),
                            },
                        );
                        loop {
                            match controller_ref.submit(q.clone()) {
                                Ok(ticket) => {
                                    submit_tx_ref
                                        .send((submit_start, TicketEnum::Queued(ticket)))
                                        .unwrap();
                                    break;
                                }
                                Err(forthdb_world::SubmitError::Full(returned)) => {
                                    q = returned;
                                    std::thread::yield_now();
                                }
                                Err(forthdb_world::SubmitError::Closed(_)) => panic!("closed"),
                            }
                        }
                    }
                    BenchmarkControl::C_SemanticWarm => {
                        let ast = TransactionAST::new(
                            idx as u64,
                            vec![TransactionOp::Define {
                                slot: "bench_warm_slot".to_owned(),
                                subject: AtomRef::Literal("sub".to_owned()),
                                predicate: "pred".to_owned(),
                                object: AtomRef::Literal(format!("val_{idx}")),
                            }],
                        );
                        let mut intent = SemanticIntent::new(ast);
                        loop {
                            match controller_ref.submit_semantic(intent) {
                                Ok(ticket) => {
                                    submit_tx_ref
                                        .send((submit_start, TicketEnum::Semantic(ticket)))
                                        .unwrap();
                                    break;
                                }
                                Err(forthdb_world::SemanticSubmitError::Full(returned)) => {
                                    intent = returned;
                                    std::thread::yield_now();
                                }
                                Err(forthdb_world::SemanticSubmitError::Closed(_)) => {
                                    panic!("closed")
                                }
                            }
                        }
                    }
                    BenchmarkControl::D_SemanticNew => {
                        let ast = TransactionAST::new(
                            idx as u64,
                            vec![TransactionOp::Define {
                                slot: format!("bench_new_slot_{idx}"),
                                subject: AtomRef::Literal("sub".to_owned()),
                                predicate: "pred".to_owned(),
                                object: AtomRef::Literal(format!("val_{idx}")),
                            }],
                        );
                        let mut intent = SemanticIntent::new(ast);
                        loop {
                            match controller_ref.submit_semantic(intent) {
                                Ok(ticket) => {
                                    submit_tx_ref
                                        .send((submit_start, TicketEnum::Semantic(ticket)))
                                        .unwrap();
                                    break;
                                }
                                Err(forthdb_world::SemanticSubmitError::Full(returned)) => {
                                    intent = returned;
                                    std::thread::yield_now();
                                }
                                Err(forthdb_world::SemanticSubmitError::Closed(_)) => {
                                    panic!("closed")
                                }
                            }
                        }
                    }
                    BenchmarkControl::E_MixedFiftyFifty => {
                        if idx % 2 == 0 {
                            let mut q = QueuedIntent::new();
                            let ent = q.entity();
                            q.define(
                                SlotId::new("bench_slot"),
                                IntentFact {
                                    subject: IntentAtom::Temporary(ent),
                                    predicate: Predicate::new("pred"),
                                    object: IntentAtom::Literal(Literal::new(&format!(
                                        "val_{idx}"
                                    ))),
                                },
                            );
                            loop {
                                match controller_ref.submit(q.clone()) {
                                    Ok(ticket) => {
                                        submit_tx_ref
                                            .send((submit_start, TicketEnum::Queued(ticket)))
                                            .unwrap();
                                        break;
                                    }
                                    Err(forthdb_world::SubmitError::Full(returned)) => {
                                        q = returned;
                                        std::thread::yield_now();
                                    }
                                    Err(forthdb_world::SubmitError::Closed(_)) => panic!("closed"),
                                }
                            }
                        } else {
                            let ast = TransactionAST::new(
                                idx as u64,
                                vec![TransactionOp::Define {
                                    slot: format!("bench_mix_slot_{idx}"),
                                    subject: AtomRef::Literal("sub".to_owned()),
                                    predicate: "pred".to_owned(),
                                    object: AtomRef::Literal(format!("val_{idx}")),
                                }],
                            );
                            let mut intent = SemanticIntent::new(ast);
                            loop {
                                match controller_ref.submit_semantic(intent) {
                                    Ok(ticket) => {
                                        submit_tx_ref
                                            .send((submit_start, TicketEnum::Semantic(ticket)))
                                            .unwrap();
                                        break;
                                    }
                                    Err(forthdb_world::SemanticSubmitError::Full(returned)) => {
                                        intent = returned;
                                        std::thread::yield_now();
                                    }
                                    Err(forthdb_world::SemanticSubmitError::Closed(_)) => {
                                        panic!("closed")
                                    }
                                }
                            }
                        }
                    }
                    BenchmarkControl::F_SemanticAcceptingValidator => {
                        let ast = TransactionAST::new(
                            idx as u64,
                            vec![TransactionOp::Define {
                                slot: "bench_val_slot".to_owned(),
                                subject: AtomRef::Literal("sub".to_owned()),
                                predicate: "pred".to_owned(),
                                object: AtomRef::Literal(format!("val_{idx}")),
                            }],
                        );
                        let mut intent = SemanticIntent::new(ast);
                        loop {
                            match controller_ref.submit_semantic(intent) {
                                Ok(ticket) => {
                                    submit_tx_ref
                                        .send((submit_start, TicketEnum::Semantic(ticket)))
                                        .unwrap();
                                    break;
                                }
                                Err(forthdb_world::SemanticSubmitError::Full(returned)) => {
                                    intent = returned;
                                    std::thread::yield_now();
                                }
                                Err(forthdb_world::SemanticSubmitError::Closed(_)) => {
                                    panic!("closed")
                                }
                            }
                        }
                    }
                    BenchmarkControl::G_SemanticRejectingValidator => {
                        let ast = TransactionAST::new(
                            idx as u64,
                            vec![TransactionOp::Define {
                                slot: "reject_trigger".to_owned(),
                                subject: AtomRef::Literal("sub".to_owned()),
                                predicate: "pred".to_owned(),
                                object: AtomRef::Literal("val".to_owned()),
                            }],
                        );
                        let mut intent = SemanticIntent::new(ast);
                        loop {
                            match controller_ref.submit_semantic(intent) {
                                Ok(ticket) => {
                                    submit_tx_ref
                                        .send((submit_start, TicketEnum::Semantic(ticket)))
                                        .unwrap();
                                    break;
                                }
                                Err(forthdb_world::SemanticSubmitError::Full(returned)) => {
                                    intent = returned;
                                    std::thread::yield_now();
                                }
                                Err(forthdb_world::SemanticSubmitError::Closed(_)) => {
                                    panic!("closed")
                                }
                            }
                        }
                    }
                }
            }
        }));
    }

    drop(submit_tx);
    barrier.wait();

    for w in workers {
        w.join().unwrap();
    }

    let mut latencies_us = Vec::with_capacity(total_intents);
    let mut accepted_count = 0usize;
    let mut rejected_count = 0usize;

    for (start_ts, ticket_item) in submit_rx {
        match ticket_item {
            TicketEnum::Queued(t) => match t.wait().unwrap() {
                TicketOutcome::Accepted { world, .. } => {
                    accepted_count += 1;
                    latencies_us.push(start_ts.elapsed().as_micros() as f64);
                    std::hint::black_box(world);
                }
                TicketOutcome::Rejected(_) => {
                    rejected_count += 1;
                    latencies_us.push(start_ts.elapsed().as_micros() as f64);
                }
            },
            TicketEnum::Semantic(t) => match t.wait().unwrap() {
                SemanticTicketOutcome::Accepted { world, bindings } => {
                    accepted_count += 1;
                    latencies_us.push(start_ts.elapsed().as_micros() as f64);
                    std::hint::black_box((world, bindings));
                }
                SemanticTicketOutcome::Rejected(_) => {
                    rejected_count += 1;
                    latencies_us.push(start_ts.elapsed().as_micros() as f64);
                }
            },
        }
    }

    controller.flush().unwrap();
    let total_elapsed = start_time.elapsed();
    let tps = total_intents as f64 / total_elapsed.as_secs_f64();

    latencies_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let (p50, p95, p99, max_lat) = if latencies_us.is_empty() {
        (0.0, 0.0, 0.0, 0.0)
    } else {
        let p50 = latencies_us[latencies_us.len() / 2];
        let p95 = latencies_us[((latencies_us.len() as f64 * 0.95) as usize).min(latencies_us.len() - 1)];
        let p99 = latencies_us[((latencies_us.len() as f64 * 0.99) as usize).min(latencies_us.len() - 1)];
        let max_lat = latencies_us.last().copied().unwrap_or(0.0);
        (p50, p95, p99, max_lat)
    };

    // Correctness Assertions
    let final_world = database.snapshot();
    assert_eq!(
        accepted_count + rejected_count,
        total_intents,
        "No lost/duplicated intents"
    );

    if matches!(control, BenchmarkControl::G_SemanticRejectingValidator) {
        assert_eq!(accepted_count, 0);
        assert_eq!(rejected_count, total_intents);
        assert_eq!(final_world.version(), initial_base.version());
    } else {
        assert_eq!(accepted_count, total_intents);
        assert_eq!(rejected_count, 0);
        assert!(final_world.version() > initial_base.version());
    }

    println!(
        "  {:<35} | {:>9.1} TPS | P50: {:>6.1} µs | P95: {:>6.1} µs | P99: {:>6.1} µs | Max: {:>6.1} µs | Acc: {:>5} | Rej: {:>5}",
        control.label(),
        tps,
        p50,
        p95,
        p99,
        max_lat,
        accepted_count,
        rejected_count
    );
    std::io::stdout().flush().ok();
    std::io::stdout().flush().ok();
}

enum TicketEnum {
    Queued(forthdb_world::CommitTicket),
    Semantic(forthdb_world::SemanticCommitTicket),
}
