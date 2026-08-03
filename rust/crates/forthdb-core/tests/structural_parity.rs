use forthdb_core::{
    Atom, Fact, ForthDb, LegacyForthDb, Literal, Pattern, Predicate, PredicateTerm, QueryOptions,
    SlotId, Term, Variable,
};

fn fact(subject: &str, predicate: &str, object: &str) -> Fact {
    Fact::new(
        Atom::Literal(Literal::new(subject)),
        Predicate::new(predicate),
        Atom::Literal(Literal::new(object)),
    )
}

fn assert_slot_parity(shared: &ForthDb, legacy: &LegacyForthDb, slot: &SlotId) {
    assert_eq!(shared.resolve(slot), legacy.resolve(slot));
    assert_eq!(shared.definitions(slot), legacy.definitions(slot));
    assert_eq!(shared.history(slot), legacy.history(slot));
}

#[test]
fn shared_kernel_matches_legacy_define_forget_and_history() {
    let mut shared = ForthDb::new();
    let mut legacy = LegacyForthDb::new();
    let slots: Vec<_> = (0..256)
        .map(|index| SlotId::new(format!("slot/{index}")))
        .collect();

    for (index, slot) in slots.iter().enumerate() {
        let value = fact("subject", "value", &index.to_string());
        shared.define(slot.clone(), value.clone());
        legacy.define(slot.clone(), value);
    }
    for index in (0..256).step_by(3) {
        let value = fact("subject", "value", &format!("updated-{index}"));
        shared.define(slots[index].clone(), value.clone());
        legacy.define(slots[index].clone(), value);
    }
    for index in (0..256).step_by(5) {
        shared.forget(slots[index].clone());
        legacy.forget(slots[index].clone());
    }

    assert_eq!(shared.active_slot_count(), legacy.active_slot_count());
    assert_eq!(shared.record_count(), legacy.record_count());
    for slot in &slots {
        assert_slot_parity(&shared, &legacy, slot);
    }
    shared.validate().expect("shared invariants");
    legacy.validate().expect("legacy invariants");
}

#[test]
fn shared_kernel_matches_legacy_queries_and_provenance() {
    let mut shared = ForthDb::new();
    let mut legacy = LegacyForthDb::new();
    let work = shared.entity();
    assert_eq!(work, legacy.entity());
    let copy = shared.entity();
    assert_eq!(copy, legacy.entity());
    let shelf = shared.entity();
    assert_eq!(shelf, legacy.entity());

    let facts = [
        (
            SlotId::new("assertion/a"),
            Fact::new(
                Atom::Entity(work),
                Predicate::new("has_copy"),
                Atom::Entity(copy),
            ),
        ),
        (
            SlotId::new("assertion/b"),
            Fact::new(
                Atom::Entity(work),
                Predicate::new("has_copy"),
                Atom::Entity(copy),
            ),
        ),
        (
            SlotId::new("copy/location"),
            Fact::new(
                Atom::Entity(copy),
                Predicate::new("located_at"),
                Atom::Entity(shelf),
            ),
        ),
    ];
    for (slot, value) in facts {
        shared.define(slot.clone(), value.clone());
        legacy.define(slot, value);
    }

    let copy_variable = Variable::new("copy").unwrap();
    let shelf_variable = Variable::new("shelf").unwrap();
    let patterns = vec![
        Pattern::new(
            Term::Atom(Atom::Entity(work)),
            PredicateTerm::Predicate(Predicate::new("has_copy")),
            Term::Variable(copy_variable.clone()),
        ),
        Pattern::new(
            Term::Variable(copy_variable),
            PredicateTerm::Predicate(Predicate::new("located_at")),
            Term::Variable(shelf_variable),
        ),
    ];
    let options = QueryOptions {
        include_provenance: true,
        distinct: false,
        ..QueryOptions::default()
    };
    assert_eq!(shared.query(&patterns, options), legacy.query(&patterns, options));
}

#[test]
fn cloning_and_mutating_shared_kernel_preserves_base_snapshot() {
    let mut base = ForthDb::new();
    for index in 0..10_000 {
        base.define(
            SlotId::new(format!("large/{index}")),
            fact("large", "value", &index.to_string()),
        );
    }
    let base_metrics = base.structural_metrics();
    let mut candidate = base.clone();
    candidate.define(
        SlotId::new("candidate/only"),
        fact("candidate", "value", "present"),
    );

    assert!(base.resolve(&SlotId::new("candidate/only")).is_none());
    assert!(candidate.resolve(&SlotId::new("candidate/only")).is_some());
    assert_eq!(base.record_count(), 10_000);
    assert_eq!(candidate.record_count(), 10_001);
    assert!(base_metrics.log_chunks >= 9);
}
