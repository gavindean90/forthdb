use forthdb_world::semantic_isa::{
    decode_instruction_stream_frame, encode_instruction_stream_frame,
};
use forthdb_world::stack_vm::{ExecutionOutcome, IntentProgram, Workspace};
use forthdb_world::transaction_ast::{AtomRef, EntityId, TransactionAST, TransactionOp, WorldId};
use serde_json::{Value, json};
use std::hint::black_box;
use std::io::Cursor;
use std::time::Instant;

const ROUNDS: usize = 5;

#[derive(Clone, Copy)]
struct Shape {
    name: &'static str,
    build: fn() -> TransactionAST,
}

fn define_one() -> TransactionAST {
    TransactionAST::new(
        1,
        vec![TransactionOp::Define {
            slot: "book/42/status".to_owned(),
            subject: AtomRef::Entity(EntityId(42)),
            predicate: "status".to_owned(),
            object: AtomRef::Literal("available".to_owned()),
        }],
    )
}

fn conditional_write() -> TransactionAST {
    TransactionAST::new(
        1,
        vec![
            TransactionOp::ExpectObject {
                slot: "book/42/status".to_owned(),
                expected: AtomRef::Literal("available".to_owned()),
            },
            TransactionOp::Define {
                slot: "book/42/status".to_owned(),
                subject: AtomRef::Entity(EntityId(42)),
                predicate: "status".to_owned(),
                object: AtomRef::Literal("checked_out".to_owned()),
            },
        ],
    )
}

fn allocated_entity() -> TransactionAST {
    TransactionAST::new(
        1,
        vec![
            TransactionOp::Allocate {
                result: "book".to_owned(),
            },
            TransactionOp::Define {
                slot: "book/new/kind".to_owned(),
                subject: AtomRef::Symbol("book".to_owned()),
                predicate: "kind".to_owned(),
                object: AtomRef::Literal("book".to_owned()),
            },
            TransactionOp::Define {
                slot: "book/new/status".to_owned(),
                subject: AtomRef::Symbol("book".to_owned()),
                predicate: "status".to_owned(),
                object: AtomRef::Literal("available".to_owned()),
            },
            TransactionOp::Define {
                slot: "book/new/location".to_owned(),
                subject: AtomRef::Symbol("book".to_owned()),
                predicate: "located_at".to_owned(),
                object: AtomRef::Entity(EntityId(7)),
            },
        ],
    )
}

fn library_checkout() -> TransactionAST {
    TransactionAST::new(
        1,
        vec![
            TransactionOp::ExpectWorld {
                expected: WorldId(100),
            },
            TransactionOp::ExpectObject {
                slot: "copy/42/status".to_owned(),
                expected: AtomRef::Literal("available".to_owned()),
            },
            TransactionOp::Define {
                slot: "copy/42/borrower".to_owned(),
                subject: AtomRef::Entity(EntityId(42)),
                predicate: "borrowed_by".to_owned(),
                object: AtomRef::Entity(EntityId(9001)),
            },
            TransactionOp::Define {
                slot: "copy/42/status".to_owned(),
                subject: AtomRef::Entity(EntityId(42)),
                predicate: "status".to_owned(),
                object: AtomRef::Literal("checked_out".to_owned()),
            },
        ],
    )
}

fn measure<F, G>(iterations: usize, mut factory: G) -> Value
where
    F: FnMut() -> usize,
    G: FnMut() -> F,
{
    let mut elapsed_ns = Vec::with_capacity(ROUNDS);
    let mut checksum = 0usize;
    for _ in 0..ROUNDS {
        let mut operation = factory();
        let started = Instant::now();
        for _ in 0..iterations {
            checksum = checksum.wrapping_add(black_box(operation()));
        }
        elapsed_ns.push(started.elapsed().as_nanos());
    }
    elapsed_ns.sort_unstable();
    let median_elapsed_ns = elapsed_ns[ROUNDS / 2];
    let median_ns_per_transaction = median_elapsed_ns as f64 / iterations as f64;
    json!({
        "iterations_per_round": iterations,
        "rounds": ROUNDS,
        "median_elapsed_ns": median_elapsed_ns,
        "median_ns_per_transaction": median_ns_per_transaction,
        "transactions_per_second": 1_000_000_000.0 / median_ns_per_transaction,
        "sample_elapsed_ns": elapsed_ns,
        "checksum": checksum,
    })
}

fn enrich(mut measurement: Value, semantic_ops: usize, instructions: usize, bytes: usize) -> Value {
    let transactions_per_second = measurement["transactions_per_second"]
        .as_f64()
        .expect("measurement throughput");
    let object = measurement
        .as_object_mut()
        .expect("measurement is an object");
    object.insert("semantic_ops_per_transaction".to_owned(), json!(semantic_ops));
    object.insert("sisa_instructions_per_transaction".to_owned(), json!(instructions));
    object.insert("encoded_bytes_per_transaction".to_owned(), json!(bytes));
    object.insert(
        "semantic_ops_per_second".to_owned(),
        json!(transactions_per_second * semantic_ops as f64),
    );
    object.insert(
        "sisa_instructions_per_second".to_owned(),
        json!(transactions_per_second * instructions as f64),
    );
    object.insert(
        "encoded_megabytes_per_second".to_owned(),
        json!(transactions_per_second * bytes as f64 / 1_000_000.0),
    );
    measurement
}

fn main() {
    let shapes = [
        Shape {
            name: "define_one",
            build: define_one,
        },
        Shape {
            name: "conditional_write",
            build: conditional_write,
        },
        Shape {
            name: "allocated_entity_three_defines",
            build: allocated_entity,
        },
        Shape {
            name: "library_checkout",
            build: library_checkout,
        },
    ];

    let mut shape_reports = Vec::new();
    for shape in shapes {
        let exemplar = (shape.build)();
        let frame = exemplar
            .lower_to_sisa()
            .unwrap_or_else(|error| panic!("{} lowering failed: {error}", shape.name));
        let mut encoded = Vec::new();
        encode_instruction_stream_frame(&frame, &mut encoded);
        let semantic_ops = exemplar.operations.len();
        let instructions = frame.instructions.len();
        let bytes = encoded.len();

        let build = enrich(
            measure(100_000, || {
                let build = shape.build;
                move || (build)().operations.len()
            }),
            semantic_ops,
            instructions,
            bytes,
        );

        let lower_ast = exemplar.clone();
        let lower = enrich(
            measure(20_000, || {
                let ast = lower_ast.clone();
                move || {
                    ast.lower_to_sisa()
                        .expect("representative AST lowers")
                        .instructions
                        .len()
                }
            }),
            semantic_ops,
            instructions,
            bytes,
        );

        let encode_frame = frame.clone();
        let encode = enrich(
            measure(100_000, || {
                let frame = encode_frame.clone();
                let mut output = Vec::with_capacity(bytes);
                move || {
                    output.clear();
                    encode_instruction_stream_frame(&frame, &mut output);
                    output.len()
                }
            }),
            semantic_ops,
            instructions,
            bytes,
        );

        let decode_bytes = encoded.clone();
        let decode = enrich(
            measure(100_000, || {
                let bytes = decode_bytes.clone();
                move || {
                    let mut cursor = Cursor::new(bytes.as_slice());
                    let decoded = decode_instruction_stream_frame(&mut cursor)
                        .expect("representative SISA decodes");
                    decoded.instructions.len()
                }
            }),
            semantic_ops,
            instructions,
            bytes,
        );

        shape_reports.push(json!({
            "name": shape.name,
            "semantic_ops": semantic_ops,
            "sisa_instructions": instructions,
            "encoded_bytes": bytes,
            "ast_construction": build,
            "validate_and_lower": lower,
            "encode": encode,
            "decode": decode,
        }));
    }

    let representative = allocated_entity();
    let representative_frame = representative
        .lower_to_sisa()
        .expect("representative AST lowers");
    let mut representative_bytes = Vec::new();
    encode_instruction_stream_frame(&representative_frame, &mut representative_bytes);
    let semantic_ops = representative.operations.len();
    let instructions = representative_frame.instructions.len();
    let bytes = representative_bytes.len();
    let slot_count = representative_frame.dictionary.slots.len();
    let program = IntentProgram::new(
        representative_frame.local_count(),
        representative_frame.instructions().to_vec(),
    );

    let execute_iterations = 100_000usize;
    let execute = enrich(
        measure(execute_iterations, || {
            let program = program.clone();
            let record_capacity = execute_iterations * 3 + 1_024;
            let mut workspace = Workspace::with_indexes(
                slot_count,
                64,
                record_capacity,
                record_capacity,
            );
            move || match workspace.execute(&program) {
                ExecutionOutcome::Accepted => 1,
                ExecutionOutcome::Rejected(error) => {
                    panic!("representative predecoded execution rejected: {error:?}")
                }
            }
        }),
        semantic_ops,
        instructions,
        bytes,
    );

    let full_iterations = 20_000usize;
    let full_pipeline = enrich(
        measure(full_iterations, || {
            let record_capacity = full_iterations * 3 + 1_024;
            let mut workspace = Workspace::with_indexes(
                slot_count,
                64,
                record_capacity,
                record_capacity,
            );
            move || {
                let ast = allocated_entity();
                let frame = ast.lower_to_sisa().expect("full pipeline lowers");
                let mut encoded = Vec::with_capacity(bytes);
                encode_instruction_stream_frame(&frame, &mut encoded);
                let mut cursor = Cursor::new(encoded.as_slice());
                let decoded = decode_instruction_stream_frame(&mut cursor)
                    .expect("full pipeline decodes");
                let program = IntentProgram::new(
                    decoded.local_count(),
                    decoded.instructions().to_vec(),
                );
                match workspace.execute(&program) {
                    ExecutionOutcome::Accepted => encoded.len(),
                    ExecutionOutcome::Rejected(error) => {
                        panic!("full semantic pipeline rejected: {error:?}")
                    }
                }
            }
        }),
        semantic_ops,
        instructions,
        bytes,
    );

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": "observational",
            "scope": "transaction-ast-v0-to-sisa-v1-semantic-intent-pipeline",
            "profile": "release",
            "environment": {
                "os": std::env::consts::OS,
                "architecture": std::env::consts::ARCH,
                "git_sha": std::env::var("GITHUB_SHA").ok(),
                "github_run_id": std::env::var("GITHUB_RUN_ID").ok(),
            },
            "shape_profiles": shape_reports,
            "representative_pipeline": {
                "shape": "allocated_entity_three_defines",
                "predecoded_indexed_vm_execution": execute,
                "ast_build_lower_encode_decode_indexed_execute": full_pipeline,
            },
            "units": {
                "transaction": "one TransactionAST frame",
                "semantic_operation": "one TransactionOp",
                "sisa_instruction": "one fixed-width SISA VM instruction",
                "encoded_megabyte": "1,000,000 encoded bytes",
            },
        }))
        .expect("benchmark report serializes")
    );
}
