use forthdb_world::semantic_isa::InstructionStreamFrame;
use forthdb_world::stack_vm::{IntentProgram, Workspace};
use forthdb_world::transaction_ast::{AtomRef, TransactionAST, TransactionOp};
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const TOTAL_INTENTS: usize = 8_192;
const REJECT_EVERY: usize = 17;
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

// NOTE: To get uninstrumented timings, comment out the global allocator.
#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[derive(Clone, Copy, Debug)]
struct Counters {
    allocated_bytes: u64,
    allocations: u64,
    deallocations: u64,
}

impl Counters {
    fn capture() -> Self {
        Self {
            allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
            allocations: ALLOCATION_COUNT.load(Ordering::Relaxed),
            deallocations: DEALLOCATION_COUNT.load(Ordering::Relaxed),
        }
    }

    fn since(self, before: Self) -> Self {
        Self {
            allocated_bytes: self.allocated_bytes.saturating_sub(before.allocated_bytes),
            allocations: self.allocations.saturating_sub(before.allocations),
            deallocations: self.deallocations.saturating_sub(before.deallocations),
        }
    }
}

fn ast_transaction(index: usize, reject_slot: &str) -> TransactionAST {
    let mut operations = Vec::new();
    let slot = if index % REJECT_EVERY == 0 {
        reject_slot.to_string()
    } else {
        format!("phase1/accepted/{index}")
    };

    operations.push(TransactionOp::Allocate {
        result: "temp".to_string(),
    });
    operations.push(TransactionOp::Define {
        slot,
        subject: AtomRef::Symbol("temp".to_string()),
        predicate: "phase1_value".to_string(),
        object: AtomRef::Literal(index.to_string()),
    });

    if index % REJECT_EVERY == 0 {
        operations.push(TransactionOp::Reject);
    }

    TransactionAST::new(1, operations)
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
        allocated_bytes: u64::MAX,
        allocations: u64::MAX,
        deallocations: u64::MAX,
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

fn main() {
    let reject_slot = "phase1/reject";

    println!("Phase                 | min ns | med ns | max ns | allocs");
    println!("------------------------------------------------------------");

    let t = TOTAL_INTENTS as u64;

    // 1. AST Build
    let (min, med, max) = time_batch(|| {
        for i in 0..TOTAL_INTENTS {
            black_box(ast_transaction(i, reject_slot));
        }
    });
    let allocs = allocs_batch(|| {
        for i in 0..TOTAL_INTENTS {
            black_box(ast_transaction(i, reject_slot));
        }
    });
    println!(
        "AST build             | {:>6} | {:>6} | {:>6} | {:>6}",
        min / t,
        med / t,
        max / t,
        allocs.allocations / t
    );

    // Prepare ASTs for phase 2
    let mut asts = Vec::with_capacity(TOTAL_INTENTS);
    for i in 0..TOTAL_INTENTS {
        asts.push(ast_transaction(i, reject_slot));
    }

    // 2a. Validation & Lowering (One-shot)
    let (min, med, max) = time_batch(|| {
        for ast in &asts {
            black_box(ast.lower_to_sisa().unwrap());
        }
    });
    let allocs = allocs_batch(|| {
        for ast in &asts {
            black_box(ast.lower_to_sisa().unwrap());
        }
    });
    println!(
        "Lowering (One-shot)   | {:>6} | {:>6} | {:>6} | {:>6}",
        min / t,
        med / t,
        max / t,
        allocs.allocations / t
    );

    // 2b. Validation & Lowering (Pooled)
    let mut ctx = forthdb_world::transaction_ast::LoweringContext::new();
    let (min, med, max) = time_batch(|| {
        for ast in &asts {
            let frame = ast.lower_to_sisa_with(&mut ctx).unwrap();
            ctx.reclaim(frame);
        }
    });
    let allocs = allocs_batch(|| {
        for ast in &asts {
            let frame = ast.lower_to_sisa_with(&mut ctx).unwrap();
            ctx.reclaim(frame);
        }
    });
    println!(
        "Lowering (Pooled)     | {:>6} | {:>6} | {:>6} | {:>6}",
        min / t,
        med / t,
        max / t,
        allocs.allocations / t
    );

    // Prepare programs for phase 3
    let mut programs = Vec::with_capacity(TOTAL_INTENTS);
    for ast in &asts {
        let frame = ast.lower_to_sisa().unwrap();
        programs.push(IntentProgram::new(frame.local_count, frame.instructions));
    }

    // 3. VM Execution
    let mut workspace =
        Workspace::with_capacity(TOTAL_INTENTS + 1, 16, TOTAL_INTENTS, TOTAL_INTENTS);
    let (min, med, max) = time_batch(|| {
        for program in &programs {
            black_box(workspace.execute(program));
        }
    });
    let allocs = allocs_batch(|| {
        for program in &programs {
            black_box(workspace.execute(program));
        }
    });
    println!(
        "VM execution          | {:>6} | {:>6} | {:>6} | {:>6}",
        min / t,
        med / t,
        max / t,
        allocs.allocations / t
    );

    // 4. One-Shot Full Pipeline
    let mut workspace =
        Workspace::with_capacity(TOTAL_INTENTS + 1, 16, TOTAL_INTENTS, TOTAL_INTENTS);
    let (min, med, max) = time_batch(|| {
        for i in 0..TOTAL_INTENTS {
            let ast = ast_transaction(i, reject_slot);
            let frame = ast.lower_to_sisa().unwrap();
            let program = IntentProgram::new(frame.local_count, frame.instructions);
            black_box(workspace.execute(&program));
        }
    });
    let allocs = allocs_batch(|| {
        for i in 0..TOTAL_INTENTS {
            let ast = ast_transaction(i, reject_slot);
            let frame = ast.lower_to_sisa().unwrap();
            let program = IntentProgram::new(frame.local_count, frame.instructions);
            black_box(workspace.execute(&program));
        }
    });
    println!(
        "Full Pipe (One-shot)  | {:>6} | {:>6} | {:>6} | {:>6}",
        min / t,
        med / t,
        max / t,
        allocs.allocations / t
    );

    // 5. Pooled Full Pipeline
    let mut workspace =
        Workspace::with_capacity(TOTAL_INTENTS + 1, 16, TOTAL_INTENTS, TOTAL_INTENTS);
    let mut ctx = forthdb_world::transaction_ast::LoweringContext::new();
    let (min, med, max) = time_batch(|| {
        for i in 0..TOTAL_INTENTS {
            let ast = ast_transaction(i, reject_slot);
            let frame = ast.lower_to_sisa_with(&mut ctx).unwrap();
            let program = IntentProgram::new(frame.local_count, frame.instructions.clone());
            black_box(workspace.execute(&program));
            ctx.reclaim(frame);
        }
    });
    let allocs = allocs_batch(|| {
        for i in 0..TOTAL_INTENTS {
            let ast = ast_transaction(i, reject_slot);
            let frame = ast.lower_to_sisa_with(&mut ctx).unwrap();
            let program = IntentProgram::new(frame.local_count, frame.instructions.clone());
            black_box(workspace.execute(&program));
            ctx.reclaim(frame);
        }
    });
    println!(
        "Full Pipe (Pooled)    | {:>6} | {:>6} | {:>6} | {:>6}",
        min / t,
        med / t,
        max / t,
        allocs.allocations / t
    );

    // 6. Owned-as-View Full Pipeline - Pooled
    let mut workspace =
        Workspace::with_capacity(TOTAL_INTENTS + 1, 16, TOTAL_INTENTS, TOTAL_INTENTS);
    let mut ctx = forthdb_world::transaction_ast::LoweringContext::new();
    use forthdb_world::transaction_ast::{OwnedOperationSource, OperationSource, TransactionView, TransactionOpView};
    let (min, med, max) = time_batch(|| {
        for i in 0..TOTAL_INTENTS {
            let ast = ast_transaction(i, reject_slot);
            let view_ops: Vec<TransactionOpView> = (0..ast.operations.len())
                .map(|idx| OwnedOperationSource(&ast.operations).operation(idx))
                .collect();
            let view = TransactionView::borrowed(ast.namespace, &view_ops);
            let frame = view.lower_to_sisa_with(&mut ctx).unwrap();
            let program = IntentProgram::new(frame.local_count, frame.instructions.clone());
            black_box(workspace.execute(&program));
            ctx.reclaim(frame);
        }
    });
    let allocs = allocs_batch(|| {
        for i in 0..TOTAL_INTENTS {
            let ast = ast_transaction(i, reject_slot);
            let view_ops: Vec<TransactionOpView> = (0..ast.operations.len())
                .map(|idx| OwnedOperationSource(&ast.operations).operation(idx))
                .collect();
            let view = TransactionView::borrowed(ast.namespace, &view_ops);
            let frame = view.lower_to_sisa_with(&mut ctx).unwrap();
            let program = IntentProgram::new(frame.local_count, frame.instructions.clone());
            black_box(workspace.execute(&program));
            ctx.reclaim(frame);
        }
    });
    println!(
        "Owned-as-View         | {:>6} | {:>6} | {:>6} | {:>6}",
        min / t,
        med / t,
        max / t,
        allocs.allocations / t
    );

    // 7. Native Borrowed -> Owned Frame - Pooled
    // We preconstruct the views to simulate the input already being in borrowed view form.
    let mut native_views = Vec::with_capacity(TOTAL_INTENTS);
    let mut native_asts = Vec::with_capacity(TOTAL_INTENTS);
    for i in 0..TOTAL_INTENTS {
        native_asts.push(ast_transaction(i, reject_slot));
    }
    for ast in &native_asts {
        let view_ops: Vec<TransactionOpView> = (0..ast.operations.len())
            .map(|idx| OwnedOperationSource(&ast.operations).operation(idx))
            .collect();
        // Since we need the views to live as long as the benchmark, we leak them just for benchmark purposes
        let view_ops_leaked = Box::leak(view_ops.into_boxed_slice());
        native_views.push(TransactionView::borrowed(ast.namespace, view_ops_leaked));
    }
    let mut workspace =
        Workspace::with_capacity(TOTAL_INTENTS + 1, 16, TOTAL_INTENTS, TOTAL_INTENTS);
    let mut ctx = forthdb_world::transaction_ast::LoweringContext::new();
    let (min, med, max) = time_batch(|| {
        for view in &native_views {
            let frame = view.lower_to_sisa_with(&mut ctx).unwrap();
            let program = IntentProgram::new(frame.local_count, frame.instructions.clone());
            black_box(workspace.execute(&program));
            ctx.reclaim(frame);
        }
    });
    let allocs = allocs_batch(|| {
        for view in &native_views {
            let frame = view.lower_to_sisa_with(&mut ctx).unwrap();
            let program = IntentProgram::new(frame.local_count, frame.instructions.clone());
            black_box(workspace.execute(&program));
            ctx.reclaim(frame);
        }
    });
    println!(
        "Native Borrowed       | {:>6} | {:>6} | {:>6} | {:>6}",
        min / t,
        med / t,
        max / t,
        allocs.allocations / t
    );
    println!("============================================================");

    // Calculate transactions per second for the pooled full pipeline (based on median time)
    let tps = 1_000_000_000u64 / (med / t);
    println!("Pooled Transactions/Sec: {}", tps);
}
