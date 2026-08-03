use crate::{
    Atom, Binding, BoundValue, EntityId, Fact, Literal, Pattern, Predicate, PredicateTerm,
    QueryMetrics, QueryOptions, QueryResult, QueryRow, Record, RecordId, RecordKind, SlotId,
    SourceTerm, Symbol, Term, Variable,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::sync::Arc;

const LOG_CHUNK_CAPACITY: usize = 1024;
const HEAD_SHARDS: usize = 4096;
const HISTORY_SHARDS: usize = 1024;
const INDEX_SHARDS: usize = 4096;
const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StructuralMetrics {
    pub log_chunks: usize,
    pub active_head_shards: usize,
    pub slot_history_shards: usize,
    pub index_shards: usize,
}

#[derive(Clone)]
struct ChunkedLog<T> {
    chunks: Arc<Vec<Arc<Vec<Arc<T>>>>>,
    len: usize,
}

impl<T> Default for ChunkedLog<T> {
    fn default() -> Self {
        Self {
            chunks: Arc::new(Vec::new()),
            len: 0,
        }
    }
}

impl<T> ChunkedLog<T> {
    fn len(&self) -> usize {
        self.len
    }

    fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    fn get(&self, index: usize) -> &T {
        let chunk_index = index / LOG_CHUNK_CAPACITY;
        let offset = index % LOG_CHUNK_CAPACITY;
        self.chunks[chunk_index][offset].as_ref()
    }

    fn push(&mut self, value: T) -> usize {
        let index = self.len;
        let chunks = Arc::make_mut(&mut self.chunks);
        match chunks.last_mut() {
            Some(last) if last.len() < LOG_CHUNK_CAPACITY => {
                Arc::make_mut(last).push(Arc::new(value));
            }
            _ => chunks.push(Arc::new(vec![Arc::new(value)])),
        }
        self.len += 1;
        index
    }
}

#[derive(Clone)]
struct SlotHistoryNode {
    record_id: RecordId,
    previous: Option<Arc<SlotHistoryNode>>,
}

struct FnvHasher(u64);

impl Default for FnvHasher {
    fn default() -> Self {
        Self(FNV_OFFSET_BASIS)
    }
}

impl Hasher for FnvHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }
}

type FnvBuildHasher = BuildHasherDefault<FnvHasher>;
type LocalMap<K, V> = HashMap<K, V, FnvBuildHasher>;
type RecordSet = HashSet<RecordId, FnvBuildHasher>;

#[derive(Clone)]
struct ShardedMap<K, V, const SHARDS: usize> {
    shards: Arc<Vec<Option<Arc<LocalMap<K, V>>>>>,
    len: usize,
}

impl<K, V, const SHARDS: usize> Default for ShardedMap<K, V, SHARDS> {
    fn default() -> Self {
        assert!(SHARDS.is_power_of_two(), "shard count must be a power of two");
        Self {
            shards: Arc::new(vec![None; SHARDS]),
            len: 0,
        }
    }
}

impl<K, V, const SHARDS: usize> ShardedMap<K, V, SHARDS>
where
    K: Clone + Eq + Hash,
    V: Clone,
{
    fn shard_index(key: &K) -> usize {
        let mut hasher = FnvHasher::default();
        key.hash(&mut hasher);
        hasher.finish() as usize & (SHARDS - 1)
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.shards[Self::shard_index(key)]
            .as_ref()
            .and_then(|shard| shard.get(key))
    }

    fn insert(&mut self, key: K, value: V) -> Option<V> {
        let shard_index = Self::shard_index(&key);
        let root = Arc::make_mut(&mut self.shards);
        let shard = root[shard_index]
            .get_or_insert_with(|| Arc::new(LocalMap::default()));
        let previous = Arc::make_mut(shard).insert(key, value);
        if previous.is_none() {
            self.len += 1;
        }
        previous
    }

    fn remove(&mut self, key: &K) -> Option<V> {
        let shard_index = Self::shard_index(key);
        let root = Arc::make_mut(&mut self.shards);
        let shard = root[shard_index].as_mut()?;
        let previous = Arc::make_mut(shard).remove(key);
        if previous.is_some() {
            self.len -= 1;
        }
        if shard.is_empty() {
            root[shard_index] = None;
        }
        previous
    }

    fn len(&self) -> usize {
        self.len
    }

    fn populated_shards(&self) -> usize {
        self.shards.iter().filter(|shard| shard.is_some()).count()
    }

    fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.shards
            .iter()
            .filter_map(|shard| shard.as_ref())
            .flat_map(|shard| shard.iter())
    }

    fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, value)| value)
    }
}

#[derive(Clone, Default)]
struct SharedDefinitionStore {
    log: ChunkedLog<Record>,
    head: ShardedMap<SlotId, RecordId, HEAD_SHARDS>,
    slot_history: ShardedMap<SlotId, Arc<SlotHistoryNode>, HISTORY_SHARDS>,
}

impl SharedDefinitionStore {
    fn record(&self, record_id: RecordId) -> &Record {
        self.log.get(record_id.value())
    }

    fn append_slot_history(&mut self, slot: &SlotId, record_id: RecordId) {
        let previous = self.slot_history.get(slot).cloned();
        self.slot_history.insert(
            slot.clone(),
            Arc::new(SlotHistoryNode {
                record_id,
                previous,
            }),
        );
    }

    fn append_define(&mut self, slot: SlotId, fact: Fact) -> RecordId {
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
        self.append_slot_history(&slot, record_id);
        self.head.insert(slot, record_id);
        record_id
    }

    fn append_forget(&mut self, slot: SlotId) -> (RecordId, Option<RecordId>) {
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
        self.append_slot_history(&slot, record_id);
        match revealed {
            Some(revealed) => {
                self.head.insert(slot, revealed);
            }
            None => {
                self.head.remove(&slot);
            }
        }
        (record_id, revealed)
    }

    fn resolve_record(&self, slot: &SlotId) -> Option<&Record> {
        self.head.get(slot).map(|record_id| self.record(*record_id))
    }

    fn definitions(&self, slot: &SlotId) -> Vec<&Record> {
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

    fn history(&self, slot: &SlotId) -> Vec<&Record> {
        let mut ids = Vec::new();
        let mut node = self.slot_history.get(slot).cloned();
        while let Some(current) = node {
            ids.push(current.record_id);
            node = current.previous.clone();
        }
        ids.reverse();
        ids.into_iter().map(|record_id| self.record(record_id)).collect()
    }

    fn active_slot_count(&self) -> usize {
        self.head.len()
    }

    fn record_count(&self) -> usize {
        self.log.len()
    }

    fn active_record_ids(&self) -> BTreeSet<RecordId> {
        self.head.values().copied().collect()
    }
}

type Index<K> = ShardedMap<K, Arc<RecordSet>, INDEX_SHARDS>;

#[derive(Clone, Default)]
struct SharedCurrentView {
    by_subject: Index<Atom>,
    by_predicate: Index<Predicate>,
    by_object: Index<Atom>,
    by_subject_predicate: Index<(Atom, Predicate)>,
    by_subject_object: Index<(Atom, Atom)>,
    by_predicate_object: Index<(Predicate, Atom)>,
    by_exact: Index<Fact>,
}

impl SharedCurrentView {
    fn add(&mut self, record_id: RecordId, store: &SharedDefinitionStore) {
        let fact = store
            .record(record_id)
            .fact
            .as_ref()
            .expect("only definitions enter the current view");
        insert_index(&mut self.by_subject, fact.subject.clone(), record_id);
        insert_index(&mut self.by_predicate, fact.predicate.clone(), record_id);
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

    fn remove(&mut self, record_id: RecordId, store: &SharedDefinitionStore) {
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
        binding: &Binding,
        store: &SharedDefinitionStore,
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
            .flat_map(|bucket| bucket.iter().copied())
            .collect();
        record_ids.sort_unstable();
        record_ids
    }

    fn validate(&self, store: &SharedDefinitionStore) -> Result<(), String> {
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

    fn populated_shards(&self) -> usize {
        self.by_subject.populated_shards()
            + self.by_predicate.populated_shards()
            + self.by_object.populated_shards()
            + self.by_subject_predicate.populated_shards()
            + self.by_subject_object.populated_shards()
            + self.by_predicate_object.populated_shards()
            + self.by_exact.populated_shards()
    }
}

fn insert_index<K>(index: &mut Index<K>, key: K, record_id: RecordId)
where
    K: Clone + Eq + Hash,
{
    let mut bucket = index
        .get(&key)
        .cloned()
        .unwrap_or_else(|| Arc::new(RecordSet::default()));
    Arc::make_mut(&mut bucket).insert(record_id);
    index.insert(key, bucket);
}

fn remove_index<K>(index: &mut Index<K>, key: &K, record_id: RecordId)
where
    K: Clone + Eq + Hash,
{
    let Some(existing) = index.get(key).cloned() else {
        return;
    };
    let mut bucket = existing;
    Arc::make_mut(&mut bucket).remove(&record_id);
    if bucket.is_empty() {
        index.remove(key);
    } else {
        index.insert(key.clone(), bucket);
    }
}

#[derive(Clone)]
struct QueryFrame {
    binding: Binding,
    provenance: Vec<SlotId>,
}

#[derive(Clone)]
pub struct ForthDb {
    store: SharedDefinitionStore,
    view: SharedCurrentView,
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
            store: SharedDefinitionStore::default(),
            view: SharedCurrentView::default(),
            next_entity: 1,
        }
    }

    pub fn structural_metrics(&self) -> StructuralMetrics {
        StructuralMetrics {
            log_chunks: self.store.log.chunk_count(),
            active_head_shards: self.store.head.populated_shards(),
            slot_history_shards: self.store.slot_history.populated_shards(),
            index_shards: self.view.populated_shards(),
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
        let record_id = self.store.append_define(slot, fact);
        self.view.add(record_id, &self.store);
        record_id
    }

    pub fn forget(&mut self, slot: SlotId) -> RecordId {
        if let Some(current) = self.store.head.get(&slot).copied() {
            self.view.remove(current, &self.store);
        }
        let (record_id, revealed) = self.store.append_forget(slot);
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
        let mut output = Vec::<QueryFrame>::new();
        let mut first_path = Vec::<Pattern>::new();
        let mut metrics = QueryMetrics::default();
        self.walk_query(
            QueryFrame {
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
        frame: QueryFrame,
        remaining: Vec<Pattern>,
        depth: usize,
        options: QueryOptions,
        output: &mut Vec<QueryFrame>,
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
                    QueryFrame {
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
