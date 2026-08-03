use forthdb_core::{Literal, Predicate, SlotId};
use forthdb_world::stack_vm::{
    Cell, ExecutionOutcome, Instruction, IntentProgram, SlotToken, Workspace,
};
use forthdb_world::{
    Database, IntentFact, MemoryCommitStore, QueuedIntent, Validator, derive_epoch_world,
};
use serde::Serialize;
use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::fs;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const TOTAL_INTENTS: usize = 8_192;
const ROUNDS: usize = 5;
const REJECT_EVERY: usize = 17;
const STACK_REPETITIONS: usize = 64;
const FLAT_SWEEP_PAIRS: usize = 5;

struct CountingAllocator;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Serialize)]
struct EngineResult {
    median_elapsed_us: u128,
    intents_per_second: f64,
    accepted: usize,
    rejected: usize,
    active_slots: usize,
    next_entity: u64,
    hot_path_allocations: u64,
}

#[derive(Serialize)]
struct CaseResult {
    epoch_width: usize,
    epochs: usize,
    intents: usize,
    current_engine: EngineResult,
    stack_vm_phase1: EngineResult,
    stack_vm_phase2: EngineResult,
    phase1_projection_kernel_ratio: f64,
    phase2_indexed_kernel_ratio: f64,
    phase2_retained_percent: f64,
    projection_parity: bool,
}

#[derive(Serialize)]
struct Report {
    status: &'static str,
    purpose: &'static str,
    phase1_scope: &'static str,
    phase2_scope: &'static str,
    rounds: usize,
    stack_repetitions: usize,
    reject_every: usize,
    phase2_width_256_vs_16_percent: f64,
    phase2_flat_gate_passed: bool,
    zero_allocation_gate_passed: bool,
    differential_gate_passed: bool,
    cases: Vec<CaseResult>,
}

fn current_intent(index: usize, reject_slot: &SlotId) -> QueuedIntent {
    let mut intent = QueuedIntent::new();
    let entity = intent.entity();
    let slot = if index % REJECT_EVERY == 0 {
        reject_slot.clone()
    } else {
        SlotId::new(format!("phase1/accepted/{index}"))
    };
    intent.define(
        slot,
        IntentFact::new(
            entity,
            Predicate::new("phase1_value"),
            Literal::new(index.to_string()),
        ),
    );
    intent
}

fn stack_program(index: usize, reject_slot: SlotToken) -> IntentProgram {
    let slot = if index % REJECT_EVERY == 0 {
        reject_slot
    } else {
        SlotToken((index + 1) as u32)
    };
    let mut instructions = vec![
        Instruction::allocate(),
        Instruction::store_local(0),
        Instruction::load_local(0),
        Instruction::push(Cell(1)),
        Instruction::push(Cell(index as u64)),
        Instruction::define(slot),
    ];
    if index % REJECT_EVERY == 0 {
        instructions.push(Instruction::reject());
    }
    IntentProgram::new(1, instructions)
}

fn median(mut samples: Vec<u128>) -> u128 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn median_f64(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn run_current(width: usize) -> EngineResult {
    let total = TOTAL_INTENTS;
    let epochs = total / width;
    let reject_slot = SlotId::new("phase1/reject");
    let batches = (0..epochs)
        .map(|epoch| {
            (0..width)
                .map(|position| current_intent(epoch * width + position, &reject_slot))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let validator_slot = reject_slot.clone();
    let validator: Validator = Arc::new(move |candidate| {
        if candidate.resolve(&validator_slot).is_some() {
            Err("deliberate Phase 1 rejection".to_owned())
        } else {
            Ok(())
        }
    });

    let mut samples = Vec::with_capacity(ROUNDS);
    let mut allocation_samples = Vec::with_capacity(ROUNDS);
    let mut final_result = None;
    for _ in 0..ROUNDS {
        let run_batches = batches.clone();
        let validators = [validator.clone()];
        let database = Database::new(MemoryCommitStore::new()).expect("genesis is valid");
        let mut world = database.snapshot();
        let mut accepted = 0;
        let mut rejected = 0;
        let allocations_before = ALLOCATIONS.load(Ordering::Relaxed);
        let started = Instant::now();
        for intents in run_batches {
            let plan = derive_epoch_world(world, intents, &validators);
            accepted += plan
                .outcomes()
                .iter()
                .filter(|outcome| outcome.accepted().is_some())
                .count();
            rejected += plan
                .outcomes()
                .iter()
                .filter(|outcome| outcome.rejected().is_some())
                .count();
            world = plan.tail();
        }
        let elapsed = started.elapsed().as_micros().max(1);
        allocation_samples.push(
            ALLOCATIONS
                .load(Ordering::Relaxed)
                .saturating_sub(allocations_before),
        );
        black_box(world.id());
        samples.push(elapsed);
        final_result = Some((world, accepted, rejected));
    }
    let elapsed = median(samples);
    let (world, accepted, rejected) = final_result.expect("at least one round");
    EngineResult {
        median_elapsed_us: elapsed,
        intents_per_second: total as f64 * 1_000_000.0 / elapsed as f64,
        accepted,
        rejected,
        active_slots: world.active_slot_count(),
        next_entity: world.next_entity(),
        hot_path_allocations: *allocation_samples.iter().min().unwrap(),
    }
}

fn run_stack(width: usize, indexed_worlds: bool) -> EngineResult {
    let total = TOTAL_INTENTS;
    let epochs = total / width;
    let reject_slot = SlotToken(0);
    let batches = (0..epochs)
        .map(|epoch| {
            (0..width)
                .map(|position| stack_program(epoch * width + position, reject_slot))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let mut samples = Vec::with_capacity(ROUNDS);
    let mut allocation_samples = Vec::with_capacity(ROUNDS);
    let mut final_result = None;
    for _ in 0..ROUNDS {
        let started = Instant::now();
        let mut round_allocations = 0;
        for _ in 0..STACK_REPETITIONS {
            let mut workspace = if indexed_worlds {
                Workspace::with_indexes(total + 1, 16, total, total)
            } else {
                Workspace::with_capacity(total + 1, 16, total, total)
            };
            let mut accepted = 0;
            let mut rejected = 0;
            let allocations_before = ALLOCATIONS.load(Ordering::Relaxed);
            for programs in &batches {
                for program in programs {
                    match workspace.execute(program) {
                        ExecutionOutcome::Accepted => accepted += 1,
                        ExecutionOutcome::Rejected(_) => rejected += 1,
                    }
                }
                if indexed_worlds {
                    black_box(workspace.publish_epoch().expect("epoch publishes a root"));
                }
            }
            round_allocations += ALLOCATIONS
                .load(Ordering::Relaxed)
                .saturating_sub(allocations_before);
            black_box(workspace.record_count());
            final_result = Some((workspace, accepted, rejected));
        }
        let elapsed = started.elapsed().as_micros().max(1);
        samples.push(elapsed);
        allocation_samples.push(round_allocations);
    }
    let elapsed = median(samples);
    let normalized_elapsed = (elapsed / STACK_REPETITIONS as u128).max(1);
    let (workspace, accepted, rejected) = final_result.expect("at least one round");
    EngineResult {
        median_elapsed_us: normalized_elapsed,
        intents_per_second: (total * STACK_REPETITIONS) as f64 * 1_000_000.0 / elapsed as f64,
        accepted,
        rejected,
        active_slots: workspace.active_slot_count(),
        next_entity: workspace.next_entity(),
        hot_path_allocations: *allocation_samples.iter().min().unwrap(),
    }
}

fn paired_phase2_width_ratio() -> f64 {
    let mut ratios = Vec::with_capacity(FLAT_SWEEP_PAIRS);
    for pair in 0..FLAT_SWEEP_PAIRS {
        let (width_16, width_256) = if pair % 2 == 0 {
            (run_stack(16, true), run_stack(256, true))
        } else {
            let width_256 = run_stack(256, true);
            let width_16 = run_stack(16, true);
            (width_16, width_256)
        };
        ratios.push(width_256.intents_per_second / width_16.intents_per_second * 100.0);
    }
    median_f64(ratios)
}

fn main() {
    let mut cases = Vec::new();
    for width in [16, 64, 128, 256] {
        let current = run_current(width);
        let phase1 = run_stack(width, false);
        let phase2 = run_stack(width, true);
        let projection_parity = current.accepted == phase1.accepted
            && current.accepted == phase2.accepted
            && current.rejected == phase1.rejected
            && current.rejected == phase2.rejected
            && current.active_slots == phase1.active_slots
            && current.active_slots == phase2.active_slots
            && current.next_entity == phase1.next_entity
            && current.next_entity == phase2.next_entity;
        let phase1_projection_kernel_ratio = phase1.intents_per_second / current.intents_per_second;
        let phase2_indexed_kernel_ratio = phase2.intents_per_second / current.intents_per_second;
        let phase2_retained_percent = phase2.intents_per_second / phase1.intents_per_second * 100.0;
        cases.push(CaseResult {
            epoch_width: width,
            epochs: TOTAL_INTENTS / width,
            intents: TOTAL_INTENTS,
            current_engine: current,
            stack_vm_phase1: phase1,
            stack_vm_phase2: phase2,
            phase1_projection_kernel_ratio,
            phase2_indexed_kernel_ratio,
            phase2_retained_percent,
            projection_parity,
        });
    }

    let phase2_width_256_vs_16_percent = paired_phase2_width_ratio();
    let phase2_flat_gate_passed = phase2_width_256_vs_16_percent >= 80.0;
    let zero_allocation_gate_passed = cases
        .iter()
        .all(|case| case.stack_vm_phase2.hot_path_allocations == 0);
    let differential_gate_passed = cases.iter().all(|case| case.projection_parity);
    let report = Report {
        status: "ok",
        purpose: "measure clone-free stack materialization before and after layered SPO/POS/OSP deltas plus immutable POD world roots",
        phase1_scope: "slot-head semantics, temporary allocation, post-write rejection, and rollback",
        phase2_scope: "adds three permutation-delta indexes covering seven query shapes, incremental semantic hashing, and allocation-free immutable root publication; compacted query bases, host validators, durable tokens, and concurrent readers remain outside the VM",
        rounds: ROUNDS,
        stack_repetitions: STACK_REPETITIONS,
        reject_every: REJECT_EVERY,
        phase2_width_256_vs_16_percent,
        phase2_flat_gate_passed,
        zero_allocation_gate_passed,
        differential_gate_passed,
        cases,
    };
    let encoded = serde_json::to_string_pretty(&report).expect("report serializes");
    println!("{encoded}");
    if let Ok(path) = env::var("FORTHDB_STACK_VM_REPORT") {
        fs::write(path, encoded).expect("write stack VM phases report");
    }
    assert!(report.zero_allocation_gate_passed);
    assert!(report.differential_gate_passed);
}
