use crate::{
    Atom, Binding, BoundValue, EntityId, Fact, Literal, Pattern, Predicate, PredicateTerm,
    QueryMetrics, QueryOptions, QueryResult, QueryRow, Record, RecordId, RecordKind, SlotId,
    SourceTerm, Symbol, Term, Variable,
};
use im::HashSet as PersistentHashSet;
use std::collections::{BTreeSet, HashMap};
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ActiveSignature {
    count: usize,
    xor: u64,
    sum: u64,
}

impl ActiveSignature {
    fn add(&mut self, record_id: RecordId) {
        let token = record_token(record_id);
        self.count += 1;
        self.xor ^= token;
        self.sum = self.sum.wrapping_add(token);
    }

    fn remove(&mut self, record_id: RecordId) {
        let token = record_token(record_id);
        self.count -= 1;
        self.xor ^= token;
        self.sum = self.sum.wrapping_sub(token);
    }
}

fn record_token(record_id: RecordId) -> u64 {
    let mut value = record_id.value() as u64 + 0x9e3779b97f4a7c15;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
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
type RecordSet = PersistentHashSet<RecordId>;

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
    active_signature: ActiveSignature,
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

    fn replace_head(&mut self, slot: SlotId, new_head: Option<RecordId>) {
        if let Some(previous) = self.head.get(&slot).copied() {
            self.active_signature.remove(previous);
        }
        match new_head {
            Some(record_id) => {
                self.head.insert(slot, record_id);
                self.active_signature.add(record_id);
            }
            None => {
                self.head.remove(&slot);
            }
        }
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
        self.replace_head(slot, Some(record_id));
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
        self.replace_head(slot, revealed);
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
        ids.into_iter()
            .map(|record_id| self.record(record_id))
            .collect()
    }

    fn active_slot_count(&self) -> usize {
        self.head.len()
    }

    fn record_count(&self) -> usize {
        self.log.len()
    }

    fn active_signature(&self) -> ActiveSignature {
        self.active_signature
    }

    fn validate_fast(&self) -> Result<(), String> {
        if self.head.len() != self.active_signature.count {
            return Err("active-head count does not match tracked signature".to_owned());
        }
        if self.active_signature.count > self.log.len() {
            return Err("active-head count exceeds record count".to_owned());
        }
        Ok(())
    }

    fn validate_full(&self) -> Result<(), String> {
        for record_id in self.head.values() {
            let record = self.record(*record_id);
            if record.kind != RecordKind::Define || record.fact.is_none() {
                return Err("slot heads must be definition records".to_owned());
            }
        }
        let mut signature = ActiveSignature::default();
        for record_id in self.head.values() {
            signature.add(*record_id);
        }
        if signature != self.active_signature {
            return Err("active-head signature does not match full audit".to_owned());
        }
        Ok(())
    }

    fn active_record_ids(&self) -> BTreeSet<RecordId> {
        self.head.values().copied().collect()
    }
}
