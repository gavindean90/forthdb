use std::error::Error;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EntityId(u64);

impl EntityId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Entity_{}", self.0)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SlotId(String);

impl SlotId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SlotId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Literal(String);

impl Literal {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Literal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Predicate(String);

impl Predicate {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Predicate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Symbol(String);

impl Symbol {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Variable(String);

impl Variable {
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidVariable> {
        let value = value.into();
        if value.is_empty() || value.starts_with('?') {
            return Err(InvalidVariable(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Variable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "?{}", self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidVariable(String);

impl fmt::Display for InvalidVariable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "variable names must be nonempty and omit the leading '?': {:?}",
            self.0
        )
    }
}

impl Error for InvalidVariable {}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Atom {
    Entity(EntityId),
    Literal(Literal),
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Term {
    Atom(Atom),
    Variable(Variable),
}

impl From<Atom> for Term {
    fn from(value: Atom) -> Self {
        Self::Atom(value)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PredicateTerm {
    Predicate(Predicate),
    Variable(Variable),
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Fact {
    pub subject: Atom,
    pub predicate: Predicate,
    pub object: Atom,
}

impl Fact {
    pub fn new(subject: Atom, predicate: Predicate, object: Atom) -> Self {
        Self {
            subject,
            predicate,
            object,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Pattern {
    pub subject: Term,
    pub predicate: PredicateTerm,
    pub object: Term,
}

impl Pattern {
    pub fn new(subject: Term, predicate: PredicateTerm, object: Term) -> Self {
        Self {
            subject,
            predicate,
            object,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_names_match_the_reference_contract() {
        assert!(Variable::new("copy").is_ok());
        assert!(Variable::new("").is_err());
        assert!(Variable::new("?copy").is_err());
    }

    #[test]
    fn semantic_values_are_distinct_types() {
        let entity = EntityId::new(1);
        let fact = Fact::new(
            Atom::Entity(entity),
            Predicate::new("state"),
            Atom::Literal(Literal::new("ready")),
        );

        assert_eq!(fact.subject, Atom::Entity(entity));
        assert_eq!(fact.predicate.as_str(), "state");
    }
}
