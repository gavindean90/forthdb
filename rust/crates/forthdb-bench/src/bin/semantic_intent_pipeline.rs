use forthdb_world::stack_vm::{IntentProgram, Workspace, ExecutionOutcome};
use forthdb_world::transaction_ast::{AtomRef, TransactionAST, TransactionOp, LoweringContext, TransactionView, TransactionOpView, OwnedOperationSource, BorrowedOperationSource, OperationSource};
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const TOTAL_INTENTS: usize = 8_192;
const ROUNDS: usize = 50;

struct CountingAllocator;

static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOCATION_COUNT: AtomicU64 = AtomicU64::new(0);
static DEALLOCATION_COUNT: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        DEALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, old, new_size) };
        if !replacement.is_null() && new_size > old.size() {
            ALLOCATED_BYTES.fetch_add((new_size - old.size()) as u64, Ordering::Relaxed);
        }
        ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        replacement
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[derive(Clone, Copy, Debug)]
struct Counters {
    allocations: u64,
}

impl Counters {
    fn capture() -> Self {
        Self {
            allocations: ALLOCATION_COUNT.load(Ordering::Relaxed),
        }
    }

    fn since(self, before: Self) -> Self {
        Self {
            allocations: self.allocations.saturating_sub(before.allocations),
        }
    }
}

struct Lcg { seed: u32 }
impl Lcg {
    fn next_u32(&mut self) -> u32 {
        self.seed = self.seed.wrapping_mul(1664525).wrapping_add(1013904223);
        self.seed
    }
    fn next_string(&mut self, opts: &[&str]) -> String {
        let idx = (self.next_u32() as usize) % opts.len();
        opts[idx].to_string()
    }
    fn next_atom(&mut self) -> AtomRef {
        match self.next_u32() % 2 {
            0 => AtomRef::Literal(self.next_string(&["a", "b", "c", "d", "e", "f"])),
            _ => AtomRef::Symbol(self.next_string(&["temp0", "temp1", "temp2"])),
        }
    }
}

fn generate_transaction(rng: &mut Lcg, op_count: usize) -> TransactionAST {
    let slots = ["slot1", "slot2", "slot3", "slot4", "slot5", "slot6"];
    let predicates = ["is", "has", "can", "should"];
    let symbols = ["temp0", "temp1", "temp2"];

    let mut operations = Vec::with_capacity(op_count);
    
    // Allocate all local symbols up front
    for sym in &symbols {
        operations.push(TransactionOp::Allocate { result: sym.to_string() });
    }

    let remaining = op_count.saturating_sub(symbols.len());
    for _ in 0..remaining {
        let op = match rng.next_u32() % 3 {
            0 => TransactionOp::ExpectObject { 
                slot: rng.next_string(&slots), 
                expected: rng.next_atom() 
            },
            1 => TransactionOp::Define {
                slot: rng.next_string(&slots),
                subject: AtomRef::Symbol(rng.next_string(&symbols)),
                predicate: rng.next_string(&predicates),
                object: rng.next_atom(),
            },
            _ => TransactionOp::Forget { slot: rng.next_string(&slots) },
        };
        operations.push(op);
    }
    
    TransactionAST::new(rng.next_u32() as u64, operations)
}

fn time_batch<F>(mut f: F) -> (u64, u64, u64)
where
    F: FnMut(),
{
    let mut timings = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let start = Instant::now();
        f();
        let elapsed = start.elapsed().as_nanos() as u64;
        timings.push(elapsed);
    }
    timings.sort_unstable();
    (
        timings[0],
        timings[timings.len() / 2],
        timings[timings.len() - 1],
    )
}

fn allocs_batch<F>(mut f: F) -> Counters
where
    F: FnMut(),
{
    // Warmup
    for _ in 0..5 {
        f();
    }

    let mut min_counters = Counters {
        allocations: u64::MAX,
    };

    for _ in 0..ROUNDS {
        let before = Counters::capture();
        f();
        let delta = Counters::capture().since(before);
        if delta.allocations < min_counters.allocations {
            min_counters = delta;
        }
    }

    min_counters
}

fn run_matrix_benchmark(name: &str, asts: &[TransactionAST], t: u64) {
    let mut native_views = Vec::with_capacity(asts.len());
    for ast in asts {
        let view_ops: Vec<TransactionOpView> = (0..ast.operations.len())
            .map(|idx| OwnedOperationSource(&ast.operations).operation(idx))
            .collect();
        let view_ops_leaked = Box::leak(view_ops.into_boxed_slice());
        native_views.push(TransactionView::borrowed(ast.namespace, view_ops_leaked));
    }
    
    let mut programs = Vec::with_capacity(asts.len());
    for ast in asts {
        let frame = ast.lower_to_sisa().unwrap();
        programs.push(IntentProgram::new(frame.local_count, frame.instructions));
    }

    println!("\n=== {} ===", name);
    println!("Path                            | min ns | med ns | max ns | Allocations");
    println!("------------------------------------------------------------------------");
    
    // 1. Predecoded VM
    let mut workspace = Workspace::with_capacity(TOTAL_INTENTS + 1, 64, TOTAL_INTENTS * 4, TOTAL_INTENTS * 4);
    let (min, med, max) = time_batch(|| {
        let mut acc_acc = 0;
        let mut acc_rej = 0;
        let mut acc_hash = 0u64;
        for program in &programs {
            match workspace.execute(program) {
                ExecutionOutcome::Accepted => acc_acc += 1,
                ExecutionOutcome::Rejected(_) => acc_rej += 1,
            }
            acc_hash = acc_hash.wrapping_add(workspace.semantic_hash());
        }
        black_box((acc_acc, acc_rej, acc_hash));
    });
    let allocs = allocs_batch(|| {
        for program in &programs {
            black_box(workspace.execute(program));
        }
    });
    println!(
        "{:<31} | {:>6} | {:>6} | {:>6} | {:>11}",
        "Predecoded VM", min / t, med / t, max / t, allocs.allocations / t
    );
    
    // 2. Preconstructed Borrowed View -> Transient Execute
    let mut workspace = Workspace::with_capacity(TOTAL_INTENTS + 1, 64, TOTAL_INTENTS * 4, TOTAL_INTENTS * 4);
    let mut ctx = LoweringContext::with_capacity(128, 128);
    let (min, med, max) = time_batch(|| {
        let mut acc_acc = 0;
        let mut acc_rej = 0;
        let mut acc_hash = 0u64;
        for view in &native_views {
            let source = BorrowedOperationSource(view.operations);
            ctx.with_lowered(view.namespace, &source, |program| {
                match workspace.execute_instructions(program.local_count, program.instructions) {
                    ExecutionOutcome::Accepted => acc_acc += 1,
                    ExecutionOutcome::Rejected(_) => acc_rej += 1,
                }
            }).unwrap();
            acc_hash = acc_hash.wrapping_add(workspace.semantic_hash());
        }
        black_box((acc_acc, acc_rej, acc_hash));
    });
    let allocs = allocs_batch(|| {
        for view in &native_views {
            let source = BorrowedOperationSource(view.operations);
            ctx.with_lowered(view.namespace, &source, |program| {
                black_box(workspace.execute_instructions(program.local_count, program.instructions));
            }).unwrap();
        }
    });
    println!(
        "{:<31} | {:>6} | {:>6} | {:>6} | {:>11}",
        "Preconstructed Borrowed View", min / t, med / t, max / t, allocs.allocations / t
    );
    
    // 3. Construct Borrowed View -> Transient Execute
    let mut workspace = Workspace::with_capacity(TOTAL_INTENTS + 1, 64, TOTAL_INTENTS * 4, TOTAL_INTENTS * 4);
    let mut ctx = LoweringContext::with_capacity(128, 128);
    let (min, med, max) = time_batch(|| {
        let mut acc_acc = 0;
        let mut acc_rej = 0;
        let mut acc_hash = 0u64;
        for ast in asts {
            let view_ops: Vec<TransactionOpView> = (0..ast.operations.len())
                .map(|idx| OwnedOperationSource(&ast.operations).operation(idx))
                .collect();
            let view = TransactionView::borrowed(ast.namespace, &view_ops);
            let source = BorrowedOperationSource(view.operations);
            ctx.with_lowered(view.namespace, &source, |program| {
                match workspace.execute_instructions(program.local_count, program.instructions) {
                    ExecutionOutcome::Accepted => acc_acc += 1,
                    ExecutionOutcome::Rejected(_) => acc_rej += 1,
                }
            }).unwrap();
            acc_hash = acc_hash.wrapping_add(workspace.semantic_hash());
        }
        black_box((acc_acc, acc_rej, acc_hash));
    });
    let allocs = allocs_batch(|| {
        for ast in asts {
            let view_ops: Vec<TransactionOpView> = (0..ast.operations.len())
                .map(|idx| OwnedOperationSource(&ast.operations).operation(idx))
                .collect();
            let view = TransactionView::borrowed(ast.namespace, &view_ops);
            let source = BorrowedOperationSource(view.operations);
            ctx.with_lowered(view.namespace, &source, |program| {
                black_box(workspace.execute_instructions(program.local_count, program.instructions));
            }).unwrap();
        }
    });
    println!(
        "{:<31} | {:>6} | {:>6} | {:>6} | {:>11}",
        "Construct Borrowed View", min / t, med / t, max / t, allocs.allocations / t
    );
}

fn main() {
    let mut rng = Lcg { seed: 12345 };
    
    let mut random_asts = Vec::with_capacity(TOTAL_INTENTS);
    for _ in 0..TOTAL_INTENTS {
        let op_count = (rng.next_u32() % 16 + 1) as usize; // 1 to 16 ops
        let mut ast = generate_transaction(&mut rng, op_count);
        while ast.lower_to_sisa().is_err() {
            ast = generate_transaction(&mut rng, op_count);
        }
        random_asts.push(ast);
    }
    
    let mut small_asts = Vec::with_capacity(TOTAL_INTENTS);
    for _ in 0..TOTAL_INTENTS {
        let mut ast = generate_transaction(&mut rng, 2);
        while ast.lower_to_sisa().is_err() { ast = generate_transaction(&mut rng, 2); }
        small_asts.push(ast);
    }
    
    let mut large_asts = Vec::with_capacity(TOTAL_INTENTS);
    for _ in 0..TOTAL_INTENTS {
        let mut ast = generate_transaction(&mut rng, 64);
        while ast.lower_to_sisa().is_err() { ast = generate_transaction(&mut rng, 64); }
        large_asts.push(ast);
    }
    
    println!("Transactions executed per benchmark round: {}", TOTAL_INTENTS);
    run_matrix_benchmark("Mixed Size (1-16 ops)", &random_asts, TOTAL_INTENTS as u64);
    run_matrix_benchmark("Small Size (2 ops)", &small_asts, TOTAL_INTENTS as u64);
    run_matrix_benchmark("Large Size (64 ops)", &large_asts, TOTAL_INTENTS as u64);
}
