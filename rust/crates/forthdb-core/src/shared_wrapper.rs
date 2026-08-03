mod legacy {
    include!("lib.rs");
}

pub use legacy::{
    Atom, Binding, BoundValue, DefinitionStore, EntityId, Fact, InvalidVariable, Literal, Pattern,
    Predicate, PredicateTerm, QueryMetrics, QueryOptions, QueryResult, QueryRow, Record, RecordId,
    RecordKind, SlotId, SourceTerm, Symbol, Term, Variable,
};
pub use legacy::ForthDb as LegacyForthDb;

mod shared {
    include!("shared_v4.rs");
}
pub use shared::{ForthDb, StructuralMetrics};
