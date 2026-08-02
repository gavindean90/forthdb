use forthdb_core::{
    Atom, Fact, ForthDb, Literal, Pattern, Predicate, PredicateTerm, QueryOptions, SlotId,
    SourceTerm, Symbol, Term, Variable,
};

fn variable(name: &str) -> Variable {
    Variable::new(name).expect("valid test variable")
}

#[test]
fn variable_names_match_the_reference_contract() {
    assert!(Variable::new("copy").is_ok());
    assert!(Variable::new("").is_err());
    assert!(Variable::new("?copy").is_err());
}

#[test]
fn define_forget_and_history_preserve_previous_heads() {
    let mut db = ForthDb::new();
    let slot = SlotId::new("deep/state");
    db.define(
        slot.clone(),
        Fact::new(
            Atom::Literal(Literal::new("deep")),
            Predicate::new("state"),
            Atom::Literal(Literal::new("v0")),
        ),
    );
    db.define(
        slot.clone(),
        Fact::new(
            Atom::Literal(Literal::new("deep")),
            Predicate::new("state"),
            Atom::Literal(Literal::new("v1")),
        ),
    );

    assert_eq!(
        db.resolve(&slot).map(|fact| &fact.object),
        Some(&Atom::Literal(Literal::new("v1")))
    );
    db.forget(slot.clone());
    assert_eq!(
        db.resolve(&slot).map(|fact| &fact.object),
        Some(&Atom::Literal(Literal::new("v0")))
    );
    assert_eq!(
        db.history(&slot)
            .iter()
            .map(|record| record.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["define", "define", "forget"]
    );
    db.validate().expect("kernel invariants should hold");
}

#[test]
fn indexed_join_and_duplicate_provenance_match_reference_behavior() {
    let mut db = ForthDb::new();
    let work = db.entity();
    let copy = db.entity();
    let shelf = db.entity();
    db.define(
        SlotId::new("assertion/a"),
        Fact::new(
            Atom::Entity(work),
            Predicate::new("has_copy"),
            Atom::Entity(copy),
        ),
    );
    db.define(
        SlotId::new("assertion/b"),
        Fact::new(
            Atom::Entity(work),
            Predicate::new("has_copy"),
            Atom::Entity(copy),
        ),
    );
    db.define(
        SlotId::new("copy/location"),
        Fact::new(
            Atom::Entity(copy),
            Predicate::new("located_at"),
            Atom::Entity(shelf),
        ),
    );

    let patterns = vec![
        Pattern::new(
            Term::Atom(Atom::Entity(work)),
            PredicateTerm::Predicate(Predicate::new("has_copy")),
            Term::Variable(variable("copy")),
        ),
        Pattern::new(
            Term::Variable(variable("copy")),
            PredicateTerm::Predicate(Predicate::new("located_at")),
            Term::Variable(variable("shelf")),
        ),
    ];
    let distinct = db.query(&patterns, QueryOptions::default());
    assert_eq!(distinct.rows.len(), 1);
    assert_eq!(distinct.metrics.candidate_facts, 3);

    let duplicates = db.query(
        &patterns,
        QueryOptions {
            distinct: false,
            include_provenance: true,
            ..QueryOptions::default()
        },
    );
    assert_eq!(duplicates.rows.len(), 2);
    assert_eq!(
        duplicates.rows[0]
            .provenance
            .iter()
            .map(SlotId::as_str)
            .collect::<Vec<_>>(),
        vec!["copy/location", "assertion/a"]
    );
    assert_eq!(
        duplicates.rows[1]
            .provenance
            .iter()
            .map(SlotId::as_str)
            .collect::<Vec<_>>(),
        vec!["copy/location", "assertion/b"]
    );
}

#[test]
fn compiled_patterns_keep_identity_after_symbol_rebinding() {
    let mut db = ForthDb::new();
    let john = db.entity();
    let bob = db.entity();
    let other = db.entity();
    db.bind_symbol("global", Symbol::new("John"), john);
    db.bind_symbol("global", Symbol::new("Bob"), bob);
    db.define(
        SlotId::new("relationship/john-bob"),
        Fact::new(
            Atom::Entity(john),
            Predicate::new("friend"),
            Atom::Entity(bob),
        ),
    );
    let old = db
        .compile_pattern(
            "global",
            SourceTerm::Symbol(Symbol::new("John")),
            Predicate::new("friend"),
            SourceTerm::Symbol(Symbol::new("Bob")),
        )
        .expect("symbols should compile");
    db.bind_symbol("global", Symbol::new("Bob"), other);
    let new = db
        .compile_pattern(
            "global",
            SourceTerm::Symbol(Symbol::new("John")),
            Predicate::new("friend"),
            SourceTerm::Symbol(Symbol::new("Bob")),
        )
        .expect("rebound symbols should compile");
    assert_eq!(db.query(&[old], QueryOptions::default()).rows.len(), 1);
    assert_eq!(db.query(&[new], QueryOptions::default()).rows.len(), 0);
}
