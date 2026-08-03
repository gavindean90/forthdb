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
    mod std {
        pub mod collections {
            pub use ::std::collections::{BTreeSet, HashMap};
            use ::std::hash::Hash;
            use ::std::marker::PhantomData;

            #[derive(Clone)]
            pub struct HashSet<T, S = ()> {
                inner: im::HashSet<T>,
                marker: PhantomData<fn() -> S>,
            }

            impl<T, S> Default for HashSet<T, S>
            where
                T: Clone + Eq + Hash,
            {
                fn default() -> Self {
                    Self {
                        inner: im::HashSet::new(),
                        marker: PhantomData,
                    }
                }
            }

            impl<T, S> HashSet<T, S>
            where
                T: Clone + Eq + Hash,
            {
                pub fn insert(&mut self, value: T) -> bool {
                    let before = self.inner.len();
                    self.inner.insert(value);
                    self.inner.len() != before
                }

                pub fn remove(&mut self, value: &T) -> bool {
                    let before = self.inner.len();
                    self.inner.remove(value);
                    self.inner.len() != before
                }

                pub fn is_empty(&self) -> bool {
                    self.inner.is_empty()
                }

                pub fn iter(&self) -> im::hashset::Iter<'_, T> {
                    self.inner.iter()
                }
            }
        }

        pub mod hash {
            pub use ::std::hash::*;
        }

        pub mod sync {
            pub use ::std::sync::*;
        }
    }

    include!("shared.rs");
}
pub use shared::{ForthDb, StructuralMetrics};
