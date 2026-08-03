use forthdb_core::{Literal, Predicate, SlotId};
use forthdb_world::stack_vm::{
    Cell, ExecutionOutcome, Instruction, IntentProgram, SlotToken, Workspace,
};
use forthdb_world::{
    Database, IntentFact, MemoryCommitStore, QueuedIntent, Validator, derive_epoch_world,
};
use serde::Serialize;
use std::env;
use std::fs;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

const EPOCHS: usize = 32;
const ROUNDS: usize = 5;
const REJECT_EVERY: usize = 17;
const STACK_REPETITIONS: usize = 64;

#[derive(Serialize)]
struct EngineResult {
    median_elapsed_us: u128,
    intents_per_second: f64,
    accepted: usize,
    rejected: usize,
    active_slots: usize,
    next_entity: u64,
}

#[derive(Serialize)]
struct CaseResult {
    epoch_width: usize,
    epochs: usize,
    intents: usize,
    current_engine: EngineResult,
    stack_vm: EngineResult,
    projection_kernel_ratio: f64,
    projection_parity: bool,
}

#[derive(Serialize)]
struct Report {
    status: &'static str,
    purpose: &'static str,
    scope: &'static str,
    rounds: usize,
    stack_repetitions: usize,
    reject_every: usize,
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

fn run_current(width: usize) -> EngineResult {
    let total = width * EPOCHS;
    let reject_slot = SlotId::new("phase1/reject");
    let batches = (0..EPOCHS)
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
    let mut final_result = None;
    for _ in 0..ROUNDS {
        let run_batches = batches.clone();
        let validators = [validator.clone()];
        let database = Database::new(MemoryCommitStore::new()).expect("genesis is valid");
        let mut world = database.snapshot();
        let mut accepted = 0;
        let mut rejected = 0;
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
    }
}

fn run_stack(width: usize) -> EngineResult {
    let total = width * EPOCHS;
    let reject_slot = SlotToken(0);
    let batches = (0..EPOCHS)
        .map(|epoch| {
            (0..width)
                .map(|position| stack_program(epoch * width + position, reject_slot))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let mut samples = Vec::with_capacity(ROUNDS);
    let mut final_result = None;
    for _ in 0..ROUNDS {
        let started = Instant::now();
        for _ in 0..STACK_REPETITIONS {
            let mut workspace = Workspace::with_capacity(total + 1, 16, total, total);
            let mut accepted = 0;
            let mut rejected = 0;
            for programs in &batches {
                for program in programs {
                    match workspace.execute(program) {
                        ExecutionOutcome::Accepted => accepted += 1,
                        ExecutionOutcome::Rejected(_) => rejected += 1,
                    }
                }
            }
            black_box(workspace.record_count());
            final_result = Some((workspace, accepted, rejected));
        }
        let elapsed = started.elapsed().as_micros().max(1);
        samples.push(elapsed);
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
    }
}

fn main() {
    let mut cases = Vec::new();
    for width in [16, 64, 128, 256] {
        let current = run_current(width);
        let stack = run_stack(width);
        let projection_parity = current.accepted == stack.accepted
            && current.rejected == stack.rejected
            && current.active_slots == stack.active_slots
            && current.next_entity == stack.next_entity;
        let projection_kernel_ratio = stack.intents_per_second / current.intents_per_second;
        cases.push(CaseResult {
            epoch_width: width,
            epochs: EPOCHS,
            intents: width * EPOCHS,
            current_engine: current,
            stack_vm: stack,
            projection_kernel_ratio,
            projection_parity,
        });
    }

    let report = Report {
        status: "ok",
        purpose: "isolate cloned semantic materialization from durable admission",
        scope: "slot-head semantics, temporary allocation, post-write rejection, and rollback; secondary query indexes and host validators remain outside the POD VM",
        rounds: ROUNDS,
        stack_repetitions: STACK_REPETITIONS,
        reject_every: REJECT_EVERY,
        cases,
    };
    let encoded = serde_json::to_string_pretty(&report).expect("report serializes");
    println!("{encoded}");
    if let Ok(path) = env::var("FORTHDB_STACK_VM_REPORT") {
        fs::write(path, encoded).expect("write Phase 1 report");
    }
    assert!(report.cases.iter().all(|case| case.projection_parity));
}
