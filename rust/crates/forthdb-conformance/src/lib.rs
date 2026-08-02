use forthdb_core::{
    Atom, EntityId, Fact, Literal, Pattern, Predicate, PredicateTerm, Term, Variable,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::Path;

#[derive(Debug)]
pub enum FixtureError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Validation(String),
}

impl FixtureError {
    fn validation(message: impl Into<String>) -> Self {
        Self::Validation(message.into())
    }
}

impl fmt::Display for FixtureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "could not read fixture: {error}"),
            Self::Json(error) => write!(formatter, "could not parse fixture JSON: {error}"),
            Self::Validation(message) => write!(formatter, "invalid conformance fixture: {message}"),
        }
    }
}

impl Error for FixtureError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Validation(_) => None,
        }
    }
}

impl From<std::io::Error> for FixtureError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for FixtureError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[derive(Debug, Deserialize)]
pub struct KernelFixture {
    pub schema_version: u32,
    pub cases: Vec<KernelCase>,
}

impl KernelFixture {
    pub fn validate(&self) -> Result<(), FixtureError> {
        if self.schema_version != 1 {
            return Err(FixtureError::validation(format!(
                "unsupported schema_version {}; expected 1",
                self.schema_version
            )));
        }
        if self.cases.is_empty() {
            return Err(FixtureError::validation("fixture must contain at least one case"));
        }

        let mut case_names = BTreeSet::new();
        for case in &self.cases {
            require_nonempty(&case.name, "case name")?;
            if !case_names.insert(case.name.as_str()) {
                return Err(FixtureError::validation(format!(
                    "duplicate case name {:?}",
                    case.name
                )));
            }
            case.validate()?;
        }
        Ok(())
    }

    pub fn case_count(&self) -> usize {
        self.cases.len()
    }

    pub fn step_count(&self) -> usize {
        self.cases.iter().map(|case| case.steps.len()).sum()
    }
}

#[derive(Debug, Deserialize)]
pub struct KernelCase {
    pub name: String,
    pub entities: Vec<String>,
    pub steps: Vec<Step>,
}

impl KernelCase {
    fn validate(&self) -> Result<(), FixtureError> {
        if self.steps.is_empty() {
            return Err(FixtureError::validation(format!(
                "case {:?} must contain at least one step",
                self.name
            )));
        }

        let mut entities = BTreeSet::new();
        for entity in &self.entities {
            require_nonempty(entity, "entity label")?;
            if !entities.insert(entity.clone()) {
                return Err(FixtureError::validation(format!(
                    "case {:?} declares entity {:?} more than once",
                    self.name, entity
                )));
            }
        }

        let mut compiled_patterns = BTreeSet::new();
        for (index, step) in self.steps.iter().enumerate() {
            let context = format!("case {:?}, step {}", self.name, index + 1);
            step.validate(&entities, &mut compiled_patterns, &context)?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Step {
    Define {
        slot: String,
        fact: FactSpec,
    },
    Query {
        name: String,
        patterns: Vec<PatternSourceSpec>,
        #[serde(default)]
        distinct: Option<bool>,
        #[serde(default)]
        include_provenance: Option<bool>,
        #[serde(default)]
        metrics: Vec<String>,
        expect: QueryExpectation,
    },
    Resolve {
        name: String,
        slot: String,
        expect: FactSpec,
    },
    Definitions {
        name: String,
        slot: String,
        expect: Vec<FactSpec>,
    },
    Forget {
        slot: String,
    },
    HistoryKinds {
        name: String,
        slot: String,
        expect: Vec<String>,
    },
    DisplayName {
        entity: String,
        value: String,
    },
    BindSymbol {
        namespace: String,
        symbol: String,
        entity: String,
    },
    Compile {
        #[serde(rename = "as")]
        alias: String,
        namespace: String,
        subject: TermSpec,
        predicate: String,
        object: TermSpec,
    },
    DisplayNameValue {
        name: String,
        entity: String,
        expect: String,
    },
}

impl Step {
    fn validate(
        &self,
        entities: &BTreeSet<String>,
        compiled_patterns: &mut BTreeSet<String>,
        context: &str,
    ) -> Result<(), FixtureError> {
        match self {
            Self::Define { slot, fact } => {
                require_nonempty(slot, &format!("{context} slot"))?;
                fact.validate(entities, context)
            }
            Self::Query {
                name,
                patterns,
                distinct: _,
                include_provenance,
                metrics,
                expect,
            } => {
                require_nonempty(name, &format!("{context} query name"))?;
                if patterns.is_empty() {
                    return Err(FixtureError::validation(format!(
                        "{context} query must contain at least one pattern"
                    )));
                }
                for pattern in patterns {
                    pattern.validate(entities, compiled_patterns, context)?;
                }
                for metric in metrics {
                    require_nonempty(metric, &format!("{context} metric name"))?;
                }
                expect.validate(entities, context)?;

                let requested: BTreeSet<&str> = metrics.iter().map(String::as_str).collect();
                for metric in expect.metrics.keys() {
                    if !requested.contains(metric.as_str()) {
                        return Err(FixtureError::validation(format!(
                            "{context} expects metric {metric:?} without requesting it"
                        )));
                    }
                }
                if !include_provenance.unwrap_or(false)
                    && expect.rows.iter().any(|row| !row.provenance.is_empty())
                {
                    return Err(FixtureError::validation(format!(
                        "{context} expects provenance but include_provenance is not true"
                    )));
                }
                Ok(())
            }
            Self::Resolve { name, slot, expect } => {
                require_nonempty(name, &format!("{context} assertion name"))?;
                require_nonempty(slot, &format!("{context} slot"))?;
                expect.validate(entities, context)
            }
            Self::Definitions { name, slot, expect } => {
                require_nonempty(name, &format!("{context} assertion name"))?;
                require_nonempty(slot, &format!("{context} slot"))?;
                for fact in expect {
                    fact.validate(entities, context)?;
                }
                Ok(())
            }
            Self::Forget { slot } => require_nonempty(slot, &format!("{context} slot")),
            Self::HistoryKinds { name, slot, expect } => {
                require_nonempty(name, &format!("{context} assertion name"))?;
                require_nonempty(slot, &format!("{context} slot"))?;
                for kind in expect {
                    if kind != "define" && kind != "forget" {
                        return Err(FixtureError::validation(format!(
                            "{context} contains unsupported history kind {kind:?}"
                        )));
                    }
                }
                Ok(())
            }
            Self::DisplayName { entity, value } => {
                require_entity(entity, entities, context)?;
                require_nonempty(value, &format!("{context} display name"))
            }
            Self::BindSymbol {
                namespace,
                symbol,
                entity,
            } => {
                require_nonempty(namespace, &format!("{context} namespace"))?;
                require_nonempty(symbol, &format!("{context} symbol"))?;
                require_entity(entity, entities, context)
            }
            Self::Compile {
                alias,
                namespace,
                subject,
                predicate,
                object,
            } => {
                require_nonempty(alias, &format!("{context} compiled alias"))?;
                require_nonempty(namespace, &format!("{context} namespace"))?;
                require_nonempty(predicate, &format!("{context} predicate"))?;
                subject.validate(entities, true, context)?;
                object.validate(entities, true, context)?;
                if !compiled_patterns.insert(alias.clone()) {
                    return Err(FixtureError::validation(format!(
                        "{context} reuses compiled alias {alias:?}"
                    )));
                }
                Ok(())
            }
            Self::DisplayNameValue {
                name,
                entity,
                expect,
            } => {
                require_nonempty(name, &format!("{context} assertion name"))?;
                require_entity(entity, entities, context)?;
                require_nonempty(expect, &format!("{context} expected display name"))
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityRef {
    pub entity: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiteralRef {
    pub literal: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariableRef {
    pub variable: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolRef {
    pub symbol: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredicateRef {
    pub predicate: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum AtomSpec {
    Entity(EntityRef),
    Literal(LiteralRef),
}

impl AtomSpec {
    fn validate(&self, entities: &BTreeSet<String>, context: &str) -> Result<(), FixtureError> {
        match self {
            Self::Entity(reference) => require_entity(&reference.entity, entities, context),
            Self::Literal(reference) => {
                require_nonempty(&reference.literal, &format!("{context} literal"))
            }
        }
    }

    pub fn materialize(
        &self,
        entities: &BTreeMap<String, EntityId>,
    ) -> Result<Atom, FixtureError> {
        match self {
            Self::Entity(reference) => entities
                .get(&reference.entity)
                .copied()
                .map(Atom::Entity)
                .ok_or_else(|| {
                    FixtureError::validation(format!(
                        "no runtime EntityId supplied for fixture entity {:?}",
                        reference.entity
                    ))
                }),
            Self::Literal(reference) => Ok(Atom::Literal(Literal::new(&reference.literal))),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum TermSpec {
    Entity(EntityRef),
    Literal(LiteralRef),
    Variable(VariableRef),
    Symbol(SymbolRef),
}

impl TermSpec {
    fn validate(
        &self,
        entities: &BTreeSet<String>,
        symbols_allowed: bool,
        context: &str,
    ) -> Result<(), FixtureError> {
        match self {
            Self::Entity(reference) => require_entity(&reference.entity, entities, context),
            Self::Literal(reference) => {
                require_nonempty(&reference.literal, &format!("{context} literal"))
            }
            Self::Variable(reference) => Variable::new(&reference.variable)
                .map(|_| ())
                .map_err(|error| FixtureError::validation(format!("{context}: {error}"))),
            Self::Symbol(reference) if symbols_allowed => {
                require_nonempty(&reference.symbol, &format!("{context} symbol"))
            }
            Self::Symbol(_) => Err(FixtureError::validation(format!(
                "{context} may use symbols only in a compile operation"
            ))),
        }
    }

    pub fn materialize(
        &self,
        entities: &BTreeMap<String, EntityId>,
    ) -> Result<Term, FixtureError> {
        match self {
            Self::Entity(reference) => entities
                .get(&reference.entity)
                .copied()
                .map(|entity| Term::Atom(Atom::Entity(entity)))
                .ok_or_else(|| {
                    FixtureError::validation(format!(
                        "no runtime EntityId supplied for fixture entity {:?}",
                        reference.entity
                    ))
                }),
            Self::Literal(reference) => {
                Ok(Term::Atom(Atom::Literal(Literal::new(&reference.literal))))
            }
            Self::Variable(reference) => Variable::new(&reference.variable)
                .map(Term::Variable)
                .map_err(|error| FixtureError::validation(error.to_string())),
            Self::Symbol(reference) => Err(FixtureError::validation(format!(
                "symbol {:?} must be resolved before materializing a pattern",
                reference.symbol
            ))),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactSpec {
    pub subject: AtomSpec,
    pub predicate: String,
    pub object: AtomSpec,
}

impl FactSpec {
    fn validate(&self, entities: &BTreeSet<String>, context: &str) -> Result<(), FixtureError> {
        self.subject.validate(entities, context)?;
        require_nonempty(&self.predicate, &format!("{context} predicate"))?;
        self.object.validate(entities, context)
    }

    pub fn materialize(
        &self,
        entities: &BTreeMap<String, EntityId>,
    ) -> Result<Fact, FixtureError> {
        Ok(Fact::new(
            self.subject.materialize(entities)?,
            Predicate::new(&self.predicate),
            self.object.materialize(entities)?,
        ))
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InlinePatternSpec {
    pub subject: TermSpec,
    pub predicate: String,
    pub object: TermSpec,
}

impl InlinePatternSpec {
    fn validate(&self, entities: &BTreeSet<String>, context: &str) -> Result<(), FixtureError> {
        self.subject.validate(entities, false, context)?;
        require_nonempty(&self.predicate, &format!("{context} predicate"))?;
        self.object.validate(entities, false, context)
    }

    pub fn materialize(
        &self,
        entities: &BTreeMap<String, EntityId>,
    ) -> Result<Pattern, FixtureError> {
        Ok(Pattern::new(
            self.subject.materialize(entities)?,
            PredicateTerm::Predicate(Predicate::new(&self.predicate)),
            self.object.materialize(entities)?,
        ))
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledPatternRef {
    pub compiled: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum PatternSourceSpec {
    Compiled(CompiledPatternRef),
    Inline(InlinePatternSpec),
}

impl PatternSourceSpec {
    fn validate(
        &self,
        entities: &BTreeSet<String>,
        compiled_patterns: &BTreeSet<String>,
        context: &str,
    ) -> Result<(), FixtureError> {
        match self {
            Self::Compiled(reference) => {
                require_nonempty(&reference.compiled, &format!("{context} compiled alias"))?;
                if !compiled_patterns.contains(&reference.compiled) {
                    return Err(FixtureError::validation(format!(
                        "{context} refers to compiled pattern {:?} before it is defined",
                        reference.compiled
                    )));
                }
                Ok(())
            }
            Self::Inline(pattern) => pattern.validate(entities, context),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum ExpectedValueSpec {
    Entity(EntityRef),
    Literal(LiteralRef),
    Predicate(PredicateRef),
}

impl ExpectedValueSpec {
    fn validate(&self, entities: &BTreeSet<String>, context: &str) -> Result<(), FixtureError> {
        match self {
            Self::Entity(reference) => require_entity(&reference.entity, entities, context),
            Self::Literal(reference) => {
                require_nonempty(&reference.literal, &format!("{context} literal"))
            }
            Self::Predicate(reference) => {
                require_nonempty(&reference.predicate, &format!("{context} predicate"))
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedRow {
    pub binding: BTreeMap<String, ExpectedValueSpec>,
    #[serde(default)]
    pub provenance: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryExpectation {
    pub rows: Vec<ExpectedRow>,
    #[serde(default)]
    pub metrics: BTreeMap<String, u64>,
}

impl QueryExpectation {
    fn validate(&self, entities: &BTreeSet<String>, context: &str) -> Result<(), FixtureError> {
        for row in &self.rows {
            for (name, value) in &row.binding {
                require_nonempty(name, &format!("{context} binding name"))?;
                value.validate(entities, context)?;
            }
            for slot in &row.provenance {
                require_nonempty(slot, &format!("{context} provenance slot"))?;
            }
        }
        for metric in self.metrics.keys() {
            require_nonempty(metric, &format!("{context} metric name"))?;
        }
        Ok(())
    }
}

pub fn load_fixture(path: impl AsRef<Path>) -> Result<KernelFixture, FixtureError> {
    let contents = fs::read_to_string(path)?;
    let fixture: KernelFixture = serde_json::from_str(&contents)?;
    fixture.validate()?;
    Ok(fixture)
}

fn require_nonempty(value: &str, context: &str) -> Result<(), FixtureError> {
    if value.is_empty() {
        return Err(FixtureError::validation(format!(
            "{context} must not be empty"
        )));
    }
    Ok(())
}

fn require_entity(
    entity: &str,
    entities: &BTreeSet<String>,
    context: &str,
) -> Result<(), FixtureError> {
    require_nonempty(entity, &format!("{context} entity reference"))?;
    if !entities.contains(entity) {
        return Err(FixtureError::validation(format!(
            "{context} refers to undeclared entity {entity:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn checked_in_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../conformance/v1/kernel_cases.json")
    }

    #[test]
    fn parses_and_validates_checked_in_v1_fixture() {
        let fixture = load_fixture(checked_in_fixture()).expect("v1 fixture should parse");
        assert_eq!(fixture.schema_version, 1);
        assert_eq!(fixture.case_count(), 4);
        assert_eq!(fixture.step_count(), 33);
    }

    #[test]
    fn materializes_fixture_labels_into_runtime_ids() {
        let fixture = load_fixture(checked_in_fixture()).expect("v1 fixture should parse");
        let Step::Define { fact, .. } = &fixture.cases[0].steps[0] else {
            panic!("first fixture step should be a definition");
        };
        let entities = BTreeMap::from([
            ("work".to_owned(), EntityId::new(10)),
            ("copy_1".to_owned(), EntityId::new(20)),
        ]);

        let actual = fact.materialize(&entities).expect("fact should materialize");
        let expected = Fact::new(
            Atom::Entity(EntityId::new(10)),
            Predicate::new("has_copy"),
            Atom::Entity(EntityId::new(20)),
        );
        assert_eq!(actual, expected);
    }
}
