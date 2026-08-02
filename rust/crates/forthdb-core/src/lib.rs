use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RecordId(usize);

impl RecordId {
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    pub const fn value(self) -> usize {
        self.0
    }
}

impl fmt::Display for RecordId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Record_{}", self.0)
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
pub enum SourceTerm {
    Atom(Atom),
    Variable(Variable),
    Symbol(Symbol),
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

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum BoundValue {
    Entity(EntityId),
    Literal(Literal),
    Predicate(Predicate),
}

impl From<Atom> for BoundValue {
    fn from(value: Atom) -> Self {
        match value {
            Atom::Entity(entity) => Self::Entity(entity),
            Atom::Literal(literal) => Self::Literal(literal),
        }
    }
}

impl BoundValue {
    pub fn as_atom(&self) -> Option<Atom> {
        match self {
            Self::Entity(entity) => Some(Atom::Entity(*entity)),
            Self::Literal(literal) => Some(Atom::Literal(literal.clone())),
            Self::Predicate(_) => None,
        }
    }

    pub fn as_predicate(&self) -> Option<Predicate> {
        match self {
            Self::Predicate(predicate) => Some(predicate.clone()),
            Self::Entity(_) | Self::Literal(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordKind {
    Define,
    Forget,
}

impl RecordKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Define => "define",
            Self::Forget => "forget",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Record {
    pub id: RecordId,
    pub kind: RecordKind,
    pub slot: SlotId,
    pub fact: Option<Fact>,
    pub previous_head: Option<RecordId>,
    pub resulting_head: Option<RecordId>,
}

#[derive(Default)]
pub struct DefinitionStore {
    log: Vec<Record>,
    head: HashMap<SlotId, RecordId>,
    slot_history: HashMap<SlotId, Vec<RecordId>>,
}

impl DefinitionStore {
    pub fn record(&self, record_id: RecordId) -> &Record {
        &self.log[record_id.value()]
    }

    fn append_define(&mut self, slot: SlotId, fact: Fact) -> (RecordId, Option<RecordId>) {
        let previous_head = self.head.get(&slot).copied();
        let record_id = RecordId::new(self.log.len());
        self.log.push(Record {
            id: record_id,
            kind: RecordKind::Define,
            slot: slot.clone(),
            fact: Some(fact),
            previous_head,
            resulting_head: Some(record_id),
        });
        self.slot_history
            .entry(slot.clone())
            .or_default()
            .push(record_id);
        self.head.insert(slot, record_id);
        (record_id, previous_head)
    }

    fn append_forget(
        &mut self,
        slot: SlotId,
    ) -> (RecordId, Option<RecordId>, Option<RecordId>) {
        let current = self.head.get(&slot).copied();
        let revealed = current.and_then(|record_id| self.record(record_id).previous_head);
        let record_id = RecordId::new(self.log.len());
        self.log.push(Record {
            id: record_id,
            kind: RecordKind::Forget,
            slot: slot.clone(),
            fact: None,
            previous_head: current,
            resulting_head: revealed,
        });
        self.slot_history
            .entry(slot.clone())
            .or_default()
            .push(record_id);
        match revealed {
            Some(revealed) => {
                self.head.insert(slot, revealed);
            }
            None => {
                self.head.remove(&slot);
            }
        }
        (record_id, current, revealed)
    }

    pub fn resolve_record(&self, slot: &SlotId) -> Option<&Record> {
        self.head.get(slot).map(|record_id| self.record(*record_id))
    }

    pub fn definitions(&self, slot: &SlotId) -> Vec<&Record> {
        let mut records = Vec::new();
        let mut record_id = self.head.get(slot).copied();
        while let Some(current) = record_id {
            let record = self.record(current);
            debug_assert_eq!(record.kind, RecordKind::Define);
            records.push(record);
            record_id = record.previous_head;
        }
        records
    }

    pub fn history(&self, slot: &SlotId) -> Vec<&Record> {
        self.slot_history
            .get(slot)
            .into_iter()
            .flatten()
            .map(|record_id| self.record(*record_id))
            .collect()
    }

    pub fn active_slot_count(&self) -> usize {
        self.head.len()
    }

    pub fn record_count(&self) -> usize {
        self.log.len()
    }

    fn active_record_ids(&self) -> BTreeSet<RecordId> {
        self.head.values().copied().collect()
    }
}

#[derive(Default)]
struct CurrentView {
    by_subject: HashMap<Atom, HashSet<RecordId>>,
    by_predicate: HashMap<Predicate, HashSet<RecordId>>,
    by_object: HashMap<Atom, HashSet<RecordId>>,
    by_subject_predicate: HashMap<(Atom, Predicate), HashSet<RecordId>>,
    by_subject_object: HashMap<(Atom, Atom), HashSet<RecordId>>,
    by_predicate_object: HashMap<(Predicate, Atom), HashSet<RecordId>>,
    by_exact: HashMap<Fact, HashSet<RecordId>>,
}

impl CurrentView {
    fn add(&mut self, record_id: RecordId, store: &DefinitionStore) {
        let fact = store
            .record(record_id)
            .fact
            .as_ref()
            .expect("only definitions enter the current view");
        insert_index(&mut self.by_subject, fact.subject.clone(), record_id);
        insert_index(
            &mut self.by_predicate,
            fact.predicate.clone(),
            record_id,
        );
        insert_index(&mut self.by_object, fact.object.clone(), record_id);
        insert_index(
            &mut self.by_subject_predicate,
            (fact.subject.clone(), fact.predicate.clone()),
            record_id,
        );
        insert_index(
            &mut self.by_subject_object,
            (fact.subject.clone(), fact.object.clone()),
            record_id,
        );
        insert_index(
            &mut self.by_predicate_object,
            (fact.predicate.clone(), fact.object.clone()),
            record_id,
        );
        insert_index(&mut self.by_exact, fact.clone(), record_id);
    }

    fn remove(&mut self, record_id: RecordId, store: &DefinitionStore) {
        let fact = store
            .record(record_id)
            .fact
            .as_ref()
            .expect("current definitions have facts");
        remove_index(&mut self.by_subject, &fact.subject, record_id);
        remove_index(&mut self.by_predicate, &fact.predicate, record_id);
        remove_index(&mut self.by_object, &fact.object, record_id);
        remove_index(
            &mut self.by_subject_predicate,
            &(fact.subject.clone(), fact.predicate.clone()),
            record_id,
        );
        remove_index(
            &mut self.by_subject_object,
            &(fact.subject.clone(), fact.object.clone()),
            record_id,
        );
        remove_index(
            &mut self.by_predicate_object,
            &(fact.predicate.clone(), fact.object.clone()),
            record_id,
        );
        remove_index(&mut self.by_exact, fact, record_id);
    }

    fn candidates(
        &self,
        pattern: &Pattern,
        binding: &BTreeMap<String, BoundValue>,
        store: &DefinitionStore,
    ) -> Vec<RecordId> {
        let subject = resolved_atom(&pattern.subject, binding);
        let predicate = resolved_predicate(&pattern.predicate, binding);
        let object = resolved_atom(&pattern.object, binding);

        let values = match (subject, predicate, object) {
            (Some(subject), Some(predicate), Some(object)) => self
                .by_exact
                .get(&Fact::new(subject, predicate, object)),
            (Some(subject), Some(predicate), None) => {
                self.by_subject_predicate.get(&(subject, predicate))
            }
            (Some(subject), None, Some(object)) => {
                self.by_subject_object.get(&(subject, object))
            }
            (None, Some(predicate), Some(object)) => {
                self.by_predicate_object.get(&(predicate, object))
            }
            (Some(subject), None, None) => self.by_subject.get(&subject),
            (None, Some(predicate), None) => self.by_predicate.get(&predicate),
            (None, None, Some(object)) => self.by_object.get(&object),
            (None, None, None) => {
                return store.active_record_ids().into_iter().collect();
            }
        };

        let mut record_ids: Vec<_> = values
            .into_iter()
            .flatten()
            .copied()
            .collect();
        record_ids.sort_unstable();
        record_ids
    }

    fn validate(&self, store: &DefinitionStore) -> Result<(), String> {
        let active = store.active_record_ids();
        let indexed: BTreeSet<_> = self
            .by_subject_predicate
            .values()
            .flat_map(|bucket| bucket.iter().copied())
            .collect();
        if active != indexed {
            return Err("current indexes do not match active slot heads".to_owned());
        }
        Ok(())
    }
}

fn insert_index<K: Eq + std::hash::Hash>(
    index: &mut HashMap<K, HashSet<RecordId>>,
    key: K,
    record_id: RecordId,
) {
    index.entry(key).or_default().insert(record_id);
}

fn remove_index<K: Eq + std::hash::Hash>(
    index: &mut HashMap<K, HashSet<RecordId>>,
    key: &K,
    record_id: RecordId,
) {
    let remove_bucket = match index.get_mut(key) {
        Some(bucket) => {
            bucket.remove(&record_id);
            bucket.is_empty()
        }
        None => false,
    };
    if remove_bucket {
        index.remove(key);
    }
}

pub type Binding = BTreeMap<String, BoundValue>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct QueryMetrics {
    pub candidate_facts: u64,
    pub bindings_emitted: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryRow {
    pub binding: Binding,
    pub provenance: Vec<SlotId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryResult {
    pub rows: Vec<QueryRow>,
    pub chosen_first_path: Vec<Pattern>,
    pub metrics: QueryMetrics,
}

#[derive(Clone, Copy, Debug)]
pub struct QueryOptions {
    pub optimize: bool,
    pub distinct: bool,
    pub include_provenance: bool,
    pub limit: Option<usize>,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            optimize: true,
            distinct: true,
            include_provenance: false,
            limit: None,
        }
    }
}

#[derive(Clone)]
struct Frame {
    binding: Binding,
    provenance: Vec<SlotId>,
}

pub struct ForthDb {
    store: DefinitionStore,
    view: CurrentView,
    next_entity: u64,
}

impl Default for ForthDb {
    fn default() -> Self {
        Self::new()
    }
}

impl ForthDb {
    pub fn new() -> Self {
        Self {
            store: DefinitionStore::default(),
            view: CurrentView::default(),
            next_entity: 1,
        }
    }

    pub fn entity(&mut self) -> EntityId {
        let entity = EntityId::new(self.next_entity);
        self.next_entity += 1;
        entity
    }

    pub fn define(&mut self, slot: SlotId, fact: Fact) -> RecordId {
        if let Some(old_head) = self.store.head.get(&slot).copied() {
            self.view.remove(old_head, &self.store);
        }
        let (record_id, _) = self.store.append_define(slot, fact);
        self.view.add(record_id, &self.store);
        record_id
    }

    pub fn forget(&mut self, slot: SlotId) -> RecordId {
        if let Some(current) = self.store.head.get(&slot).copied() {
            self.view.remove(current, &self.store);
        }
        let (record_id, _, revealed) = self.store.append_forget(slot);
        if let Some(revealed) = revealed {
            self.view.add(revealed, &self.store);
        }
        record_id
    }

    pub fn resolve(&self, slot: &SlotId) -> Option<&Fact> {
        self.store
            .resolve_record(slot)
            .and_then(|record| record.fact.as_ref())
    }

    pub fn definitions(&self, slot: &SlotId) -> Vec<&Fact> {
        self.store
            .definitions(slot)
            .into_iter()
            .filter_map(|record| record.fact.as_ref())
            .collect()
    }

    pub fn history(&self, slot: &SlotId) -> Vec<&Record> {
        self.store.history(slot)
    }

    pub fn active_slot_count(&self) -> usize {
        self.store.active_slot_count()
    }

    pub fn record_count(&self) -> usize {
        self.store.record_count()
    }

    pub fn display_slot(entity: EntityId) -> SlotId {
        SlotId::new(format!("display/{}", entity.value()))
    }

    pub fn symbol_slot(namespace: &str, symbol: &Symbol) -> SlotId {
        SlotId::new(format!("namespace/{namespace}/{}", symbol.as_str()))
    }

    pub fn define_display_name(&mut self, entity: EntityId, name: impl Into<String>) -> RecordId {
        self.define(
            Self::display_slot(entity),
            Fact::new(
                Atom::Entity(entity),
                Predicate::new("display_name"),
                Atom::Literal(Literal::new(name)),
            ),
        )
    }

    pub fn display_name(&self, entity: EntityId) -> String {
        self.resolve(&Self::display_slot(entity))
            .and_then(|fact| match &fact.object {
                Atom::Literal(value) => Some(value.as_str().to_owned()),
                Atom::Entity(_) => None,
            })
            .unwrap_or_else(|| entity.to_string())
    }

    pub fn bind_symbol(
        &mut self,
        namespace: &str,
        symbol: Symbol,
        entity: EntityId,
    ) -> RecordId {
        self.define(
            Self::symbol_slot(namespace, &symbol),
            Fact::new(
                Atom::Literal(Literal::new(format!(
                    "{namespace}:{}",
                    symbol.as_str()
                ))),
                Predicate::new("resolves_to"),
                Atom::Entity(entity),
            ),
        )
    }

    pub fn resolve_symbol(&self, namespace: &str, symbol: &Symbol) -> Option<EntityId> {
        self.resolve(&Self::symbol_slot(namespace, symbol))
            .and_then(|fact| match fact.object {
                Atom::Entity(entity) => Some(entity),
                Atom::Literal(_) => None,
            })
    }

    pub fn compile_pattern(
        &self,
        namespace: &str,
        subject: SourceTerm,
        predicate: Predicate,
        object: SourceTerm,
    ) -> Result<Pattern, String> {
        Ok(Pattern::new(
            self.compile_term(namespace, subject)?,
            PredicateTerm::Predicate(predicate),
            self.compile_term(namespace, object)?,
        ))
    }

    fn compile_term(&self, namespace: &str, term: SourceTerm) -> Result<Term, String> {
        match term {
            SourceTerm::Atom(atom) => Ok(Term::Atom(atom)),
            SourceTerm::Variable(variable) => Ok(Term::Variable(variable)),
            SourceTerm::Symbol(symbol) => self
                .resolve_symbol(namespace, &symbol)
                .map(|entity| Term::Atom(Atom::Entity(entity)))
                .ok_or_else(|| format!("unbound symbol {namespace}:{}", symbol.as_str())),
        }
    }

    pub fn query(&self, patterns: &[Pattern], options: QueryOptions) -> QueryResult {
        let mut output = Vec::<Frame>::new();
        let mut first_path = Vec::<Pattern>::new();
        let mut metrics = QueryMetrics::default();
        self.walk_query(
            Frame {
                binding: Binding::new(),
                provenance: Vec::new(),
            },
            patterns.to_vec(),
            0,
            options,
            &mut output,
            &mut first_path,
            &mut metrics,
        );

        if options.distinct {
            let mut seen = BTreeSet::new();
            output.retain(|frame| seen.insert(frame.binding.clone()));
        }

        output.sort_by(|left, right| {
            left.binding.cmp(&right.binding).then_with(|| {
                left.provenance
                    .iter()
                    .map(SlotId::as_str)
                    .cmp(right.provenance.iter().map(SlotId::as_str))
            })
        });

        let rows = output
            .into_iter()
            .map(|frame| QueryRow {
                binding: frame.binding,
                provenance: if options.include_provenance {
                    frame.provenance
                } else {
                    Vec::new()
                },
            })
            .collect();

        QueryResult {
            rows,
            chosen_first_path: first_path,
            metrics,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_query(
        &self,
        frame: Frame,
        remaining: Vec<Pattern>,
        depth: usize,
        options: QueryOptions,
        output: &mut Vec<Frame>,
        first_path: &mut Vec<Pattern>,
        metrics: &mut QueryMetrics,
    ) -> bool {
        if remaining.is_empty() {
            output.push(frame);
            return options.limit.is_some_and(|limit| output.len() >= limit);
        }

        let chosen_index = if options.optimize {
            remaining
                .iter()
                .enumerate()
                .min_by_key(|(_, pattern)| {
                    self.view
                        .candidates(pattern, &frame.binding, &self.store)
                        .len()
                })
                .map(|(index, _)| index)
                .unwrap_or(0)
        } else {
            0
        };
        let chosen = remaining[chosen_index].clone();
        if depth == first_path.len() {
            first_path.push(chosen.clone());
        }
        let mut rest = remaining;
        rest.remove(chosen_index);

        let candidates = self
            .view
            .candidates(&chosen, &frame.binding, &self.store);
        metrics.candidate_facts += candidates.len() as u64;
        for record_id in candidates {
            let record = self.store.record(record_id);
            let fact = record
                .fact
                .as_ref()
                .expect("indexed records are definitions");
            if let Some(binding) = unify_pattern(&chosen, fact, &frame.binding) {
                metrics.bindings_emitted += 1;
                let mut provenance = frame.provenance.clone();
                provenance.push(record.slot.clone());
                if self.walk_query(
                    Frame {
                        binding,
                        provenance,
                    },
                    rest.clone(),
                    depth + 1,
                    options,
                    output,
                    first_path,
                    metrics,
                ) {
                    return true;
                }
            }
        }
        false
    }

    pub fn render_value(&self, value: &BoundValue) -> String {
        match value {
            BoundValue::Entity(entity) => self.display_name(*entity),
            BoundValue::Literal(literal) => literal.as_str().to_owned(),
            BoundValue::Predicate(predicate) => predicate.as_str().to_owned(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        for record_id in self.store.head.values() {
            let record = self.store.record(*record_id);
            if record.kind != RecordKind::Define || record.fact.is_none() {
                return Err("slot heads must be definition records".to_owned());
            }
        }
        self.view.validate(&self.store)
    }
}

fn resolved_atom(term: &Term, binding: &Binding) -> Option<Atom> {
    match term {
        Term::Atom(atom) => Some(atom.clone()),
        Term::Variable(variable) => binding.get(variable.as_str()).and_then(BoundValue::as_atom),
    }
}

fn resolved_predicate(term: &PredicateTerm, binding: &Binding) -> Option<Predicate> {
    match term {
        PredicateTerm::Predicate(predicate) => Some(predicate.clone()),
        PredicateTerm::Variable(variable) => binding
            .get(variable.as_str())
            .and_then(BoundValue::as_predicate),
    }
}

fn unify_pattern(pattern: &Pattern, fact: &Fact, binding: &Binding) -> Option<Binding> {
    let binding = unify_term(&pattern.subject, BoundValue::from(fact.subject.clone()), binding)?;
    let binding = unify_predicate(
        &pattern.predicate,
        BoundValue::Predicate(fact.predicate.clone()),
        &binding,
    )?;
    unify_term(&pattern.object, BoundValue::from(fact.object.clone()), &binding)
}

fn unify_term(term: &Term, value: BoundValue, binding: &Binding) -> Option<Binding> {
    match term {
        Term::Atom(atom) => {
            if value.as_atom().as_ref() == Some(atom) {
                Some(binding.clone())
            } else {
                None
            }
        }
        Term::Variable(variable) => unify_variable(variable, value, binding),
    }
}

fn unify_predicate(
    term: &PredicateTerm,
    value: BoundValue,
    binding: &Binding,
) -> Option<Binding> {
    match term {
        PredicateTerm::Predicate(predicate) => {
            if value.as_predicate().as_ref() == Some(predicate) {
                Some(binding.clone())
            } else {
                None
            }
        }
        PredicateTerm::Variable(variable) => unify_variable(variable, value, binding),
    }
}

fn unify_variable(variable: &Variable, value: BoundValue, binding: &Binding) -> Option<Binding> {
    match binding.get(variable.as_str()) {
        Some(existing) if existing == &value => Some(binding.clone()),
        Some(_) => None,
        None => {
            let mut extended = binding.clone();
            extended.insert(variable.as_str().to_owned(), value);
            Some(extended)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(duplicates.rows[0].provenance[0].as_str(), "assertion/a");
        assert_eq!(duplicates.rows[1].provenance[0].as_str(), "assertion/b");
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
}
