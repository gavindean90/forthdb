use forthdb_conformance::{
    ExpectedRow, ExpectedValueSpec, FixtureError, KernelCase, KernelFixture, PatternSourceSpec,
    Step, TermSpec,
};
use forthdb_core::{
    Atom, Binding, BoundValue, EntityId, ForthDb, Literal, Pattern, Predicate, QueryOptions,
    QueryRow, SlotId, SourceTerm, Symbol, Variable,
};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Serialize)]
pub struct CaseReport {
    pub name: String,
    pub assertions: usize,
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ExecutionReport {
    pub implementation: &'static str,
    pub scope: &'static str,
    pub schema_version: u32,
    pub cases: usize,
    pub steps: usize,
    pub assertions: usize,
    pub case_reports: Vec<CaseReport>,
    pub status: &'static str,
}

pub fn execute_fixture(fixture: &KernelFixture) -> Result<ExecutionReport, FixtureError> {
    let mut case_reports = Vec::new();
    let mut assertion_total = 0;
    for case in &fixture.cases {
        let assertions = execute_case(case)?;
        assertion_total += assertions;
        case_reports.push(CaseReport {
            name: case.name.clone(),
            assertions,
            status: "passed",
        });
    }

    Ok(ExecutionReport {
        implementation: "rust",
        scope: "in-memory-semantic-kernel",
        schema_version: fixture.schema_version,
        cases: fixture.case_count(),
        steps: fixture.step_count(),
        assertions: assertion_total,
        case_reports,
        status: "passed",
    })
}

fn execute_case(case: &KernelCase) -> Result<usize, FixtureError> {
    let mut db = ForthDb::new();
    let entities: BTreeMap<String, EntityId> = case
        .entities
        .iter()
        .map(|name| (name.clone(), db.entity()))
        .collect();
    let mut compiled = BTreeMap::<String, Pattern>::new();
    let mut assertions = 0;

    for (index, step) in case.steps.iter().enumerate() {
        let context = format!("case {:?}, step {}", case.name, index + 1);
        match step {
            Step::Define { slot, fact } => {
                db.define(SlotId::new(slot), fact.materialize(&entities)?);
            }
            Step::Forget { slot } => {
                db.forget(SlotId::new(slot));
            }
            Step::Resolve { name, slot, expect } => {
                let expected = expect.materialize(&entities)?;
                let actual = db.resolve(&SlotId::new(slot));
                if actual != Some(&expected) {
                    return Err(mismatch(
                        &context,
                        name,
                        format!("{expected:?}"),
                        format!("{actual:?}"),
                    ));
                }
                assertions += 1;
            }
            Step::Definitions { name, slot, expect } => {
                let expected: Result<Vec<_>, _> = expect
                    .iter()
                    .map(|fact| fact.materialize(&entities))
                    .collect();
                let expected = expected?;
                let actual: Vec<_> = db
                    .definitions(&SlotId::new(slot))
                    .into_iter()
                    .cloned()
                    .collect();
                if actual != expected {
                    return Err(mismatch(
                        &context,
                        name,
                        format!("{expected:?}"),
                        format!("{actual:?}"),
                    ));
                }
                assertions += 1;
            }
            Step::HistoryKinds { name, slot, expect } => {
                let actual: Vec<_> = db
                    .history(&SlotId::new(slot))
                    .into_iter()
                    .map(|record| record.kind.as_str().to_owned())
                    .collect();
                if &actual != expect {
                    return Err(mismatch(
                        &context,
                        name,
                        format!("{expect:?}"),
                        format!("{actual:?}"),
                    ));
                }
                assertions += 1;
            }
            Step::DisplayName { entity, value } => {
                db.define_display_name(entity_id(entity, &entities, &context)?, value);
            }
            Step::DisplayNameValue {
                name,
                entity,
                expect,
            } => {
                let actual = db.display_name(entity_id(entity, &entities, &context)?);
                if &actual != expect {
                    return Err(mismatch(&context, name, expect.clone(), actual));
                }
                assertions += 1;
            }
            Step::BindSymbol {
                namespace,
                symbol,
                entity,
            } => {
                db.bind_symbol(
                    namespace,
                    Symbol::new(symbol),
                    entity_id(entity, &entities, &context)?,
                );
            }
            Step::Compile {
                alias,
                namespace,
                subject,
                predicate,
                object,
            } => {
                let pattern = db
                    .compile_pattern(
                        namespace,
                        source_term(subject, &entities, &context)?,
                        Predicate::new(predicate),
                        source_term(object, &entities, &context)?,
                    )
                    .map_err(|error| FixtureError::Validation(format!("{context}: {error}")))?;
                compiled.insert(alias.clone(), pattern);
            }
            Step::Query {
                name,
                patterns,
                distinct,
                include_provenance,
                metrics: _,
                expect,
            } => {
                let patterns: Result<Vec<_>, FixtureError> = patterns
                    .iter()
                    .map(|pattern| match pattern {
                        PatternSourceSpec::Inline(pattern) => pattern.materialize(&entities),
                        PatternSourceSpec::Compiled(reference) => {
                            compiled.get(&reference.compiled).cloned().ok_or_else(|| {
                                FixtureError::Validation(format!(
                                    "{context}: compiled pattern {:?} is unavailable",
                                    reference.compiled
                                ))
                            })
                        }
                    })
                    .collect();
                let result = db.query(
                    &patterns?,
                    QueryOptions {
                        distinct: distinct.unwrap_or(true),
                        include_provenance: include_provenance.unwrap_or(false),
                        ..QueryOptions::default()
                    },
                );
                compare_rows(&context, name, &result.rows, &expect.rows, &entities)?;
                for (metric, expected) in &expect.metrics {
                    let actual = match metric.as_str() {
                        "candidate_facts" => result.metrics.candidate_facts,
                        "bindings_emitted" => result.metrics.bindings_emitted,
                        unsupported => {
                            return Err(FixtureError::Validation(format!(
                                "{context}: unsupported executable metric {unsupported:?}"
                            )));
                        }
                    };
                    if actual != *expected {
                        return Err(mismatch(
                            &context,
                            name,
                            format!("{metric}={expected}"),
                            format!("{metric}={actual}"),
                        ));
                    }
                }
                assertions += 1 + expect.metrics.len();
            }
        }
    }

    db.validate()
        .map_err(|error| FixtureError::Validation(format!("case {:?}: {error}", case.name)))?;
    Ok(assertions)
}

fn source_term(
    spec: &TermSpec,
    entities: &BTreeMap<String, EntityId>,
    context: &str,
) -> Result<SourceTerm, FixtureError> {
    match spec {
        TermSpec::Entity(reference) => Ok(SourceTerm::Atom(Atom::Entity(entity_id(
            &reference.entity,
            entities,
            context,
        )?))),
        TermSpec::Literal(reference) => Ok(SourceTerm::Atom(Atom::Literal(Literal::new(
            &reference.literal,
        )))),
        TermSpec::Variable(reference) => Variable::new(&reference.variable)
            .map(SourceTerm::Variable)
            .map_err(|error| FixtureError::Validation(format!("{context}: {error}"))),
        TermSpec::Symbol(reference) => Ok(SourceTerm::Symbol(Symbol::new(&reference.symbol))),
    }
}

fn entity_id(
    name: &str,
    entities: &BTreeMap<String, EntityId>,
    context: &str,
) -> Result<EntityId, FixtureError> {
    entities.get(name).copied().ok_or_else(|| {
        FixtureError::Validation(format!("{context}: runtime entity {name:?} is unavailable"))
    })
}

fn expected_value(
    spec: &ExpectedValueSpec,
    entities: &BTreeMap<String, EntityId>,
    context: &str,
) -> Result<BoundValue, FixtureError> {
    match spec {
        ExpectedValueSpec::Entity(reference) => Ok(BoundValue::Entity(entity_id(
            &reference.entity,
            entities,
            context,
        )?)),
        ExpectedValueSpec::Literal(reference) => {
            Ok(BoundValue::Literal(Literal::new(&reference.literal)))
        }
        ExpectedValueSpec::Predicate(reference) => {
            Ok(BoundValue::Predicate(Predicate::new(&reference.predicate)))
        }
    }
}

type CanonicalRow = (Binding, Vec<String>);

fn compare_rows(
    context: &str,
    name: &str,
    actual: &[QueryRow],
    expected: &[ExpectedRow],
    entities: &BTreeMap<String, EntityId>,
) -> Result<(), FixtureError> {
    let mut actual_rows: Vec<CanonicalRow> = actual
        .iter()
        .map(|row| {
            (
                row.binding.clone(),
                row.provenance
                    .iter()
                    .map(|slot| slot.as_str().to_owned())
                    .collect(),
            )
        })
        .collect();
    let mut expected_rows: Vec<CanonicalRow> = expected
        .iter()
        .map(|row| {
            let binding: Result<Binding, FixtureError> = row
                .binding
                .iter()
                .map(|(variable, value)| {
                    Ok((variable.clone(), expected_value(value, entities, context)?))
                })
                .collect();
            Ok((binding?, row.provenance.clone()))
        })
        .collect::<Result<_, FixtureError>>()?;
    actual_rows.sort();
    expected_rows.sort();
    if actual_rows != expected_rows {
        return Err(mismatch(
            context,
            name,
            format!("{expected_rows:?}"),
            format!("{actual_rows:?}"),
        ));
    }
    Ok(())
}

fn mismatch(context: &str, name: &str, expected: String, actual: String) -> FixtureError {
    FixtureError::Validation(format!(
        "{context}, assertion {name:?} mismatch\nexpected: {expected}\nactual: {actual}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use forthdb_conformance::load_fixture;
    use std::path::PathBuf;

    fn checked_in_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../conformance/v1/kernel_cases.json")
    }

    #[test]
    fn rust_kernel_executes_every_v1_conformance_assertion() {
        let fixture = load_fixture(checked_in_fixture()).expect("fixture should parse");
        let report = execute_fixture(&fixture).expect("Rust kernel should conform");
        assert_eq!(report.cases, 4);
        assert!(report.assertions >= 10);
        assert_eq!(report.status, "passed");
    }
}
