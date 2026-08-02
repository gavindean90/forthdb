use forthdb_core::{
    Atom, EntityId, Fact, ForthDb, Literal, Pattern, Predicate, QueryOptions, QueryResult, SlotId,
    Symbol,
};
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex, RwLock};

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorldId(u64);

impl WorldId {
    pub const GENESIS: Self = Self(FNV_OFFSET_BASIS);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

impl fmt::Display for WorldId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "World_{:016x}", self.0)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Operation {
    AllocateEntity { entity: EntityId },
    Define { slot: SlotId, fact: Fact },
    Forget { slot: SlotId },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitFrame {
    parent_world: WorldId,
    resulting_world: WorldId,
    parent_version: u64,
    resulting_version: u64,
    resulting_allocator: u64,
    operations: Arc<[Operation]>,
}

impl CommitFrame {
    pub fn parent_world(&self) -> WorldId {
        self.parent_world
    }

    pub fn resulting_world(&self) -> WorldId {
        self.resulting_world
    }

    pub fn parent_version(&self) -> u64 {
        self.parent_version
    }

    pub fn resulting_version(&self) -> u64 {
        self.resulting_version
    }

    pub fn resulting_allocator(&self) -> u64 {
        self.resulting_allocator
    }

    pub fn operations(&self) -> &[Operation] {
        &self.operations
    }
}

pub trait CommitStore: Send {
    type Error: Error + Send + Sync + 'static;

    fn append(&mut self, frame: Arc<CommitFrame>) -> Result<(), Self::Error>;

    fn frames(&self) -> Vec<Arc<CommitFrame>>;

    fn len(&self) -> usize {
        self.frames().len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Default)]
pub struct MemoryCommitStore {
    frames: Vec<Arc<CommitFrame>>,
}

impl MemoryCommitStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn as_slice(&self) -> &[Arc<CommitFrame>] {
        &self.frames
    }
}

impl CommitStore for MemoryCommitStore {
    type Error = Infallible;

    fn append(&mut self, frame: Arc<CommitFrame>) -> Result<(), Self::Error> {
        self.frames.push(frame);
        Ok(())
    }

    fn frames(&self) -> Vec<Arc<CommitFrame>> {
        self.frames.clone()
    }

    fn len(&self) -> usize {
        self.frames.len()
    }
}

struct HistoryNode {
    parent: Option<Arc<HistoryNode>>,
    frame: Arc<CommitFrame>,
}

pub struct World {
    id: WorldId,
    version: u64,
    next_entity: u64,
    operation_count: usize,
    kernel: ForthDb,
    history: Option<Arc<HistoryNode>>,
}

impl fmt::Debug for World {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("World")
            .field("id", &self.id)
            .field("version", &self.version)
            .field("next_entity", &self.next_entity)
            .field("operation_count", &self.operation_count)
            .field("active_slots", &self.kernel.active_slot_count())
            .field("records", &self.kernel.record_count())
            .finish()
    }
}

impl World {
    fn genesis() -> Self {
        Self {
            id: WorldId::GENESIS,
            version: 0,
            next_entity: 1,
            operation_count: 0,
            kernel: ForthDb::new(),
            history: None,
        }
    }

    pub fn id(&self) -> WorldId {
        self.id
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn next_entity(&self) -> u64 {
        self.next_entity
    }

    pub fn operation_count(&self) -> usize {
        self.operation_count
    }

    pub fn active_slot_count(&self) -> usize {
        self.kernel.active_slot_count()
    }

    pub fn record_count(&self) -> usize {
        self.kernel.record_count()
    }

    pub fn resolve(&self, slot: &SlotId) -> Option<&Fact> {
        self.kernel.resolve(slot)
    }

    pub fn definitions(&self, slot: &SlotId) -> Vec<&Fact> {
        self.kernel.definitions(slot)
    }

    pub fn query(&self, patterns: &[Pattern], options: QueryOptions) -> QueryResult {
        self.kernel.query(patterns, options)
    }

    pub fn display_name(&self, entity: EntityId) -> String {
        self.kernel.display_name(entity)
    }

    pub fn frames(&self) -> Vec<Arc<CommitFrame>> {
        let mut frames = Vec::with_capacity(self.version as usize);
        let mut node = self.history.clone();
        while let Some(current) = node {
            frames.push(current.frame.clone());
            node = current.parent.clone();
        }
        frames.reverse();
        frames
    }

    fn reconstruct(frames: &[Arc<CommitFrame>]) -> Result<Self, CandidateError> {
        let mut world = Self::genesis();
        for frame in frames {
            if frame.parent_world != world.id {
                return Err(CandidateError::HistoryParentMismatch {
                    expected: world.id,
                    actual: frame.parent_world,
                });
            }
            if frame.parent_version != world.version {
                return Err(CandidateError::HistoryVersionMismatch {
                    expected: world.version,
                    actual: frame.parent_version,
                });
            }

            let mut next_entity = world.next_entity;
            for operation in frame.operations.iter() {
                apply_operation(&mut world.kernel, &mut next_entity, operation)?;
            }
            if next_entity != frame.resulting_allocator {
                return Err(CandidateError::AllocatorStateMismatch {
                    expected: frame.resulting_allocator,
                    actual: next_entity,
                });
            }
            world
                .kernel
                .validate()
                .map_err(CandidateError::KernelInvariant)?;

            let expected_world = calculate_world_id(
                frame.parent_world,
                frame.resulting_version,
                frame.resulting_allocator,
                frame.operations(),
            );
            if expected_world != frame.resulting_world {
                return Err(CandidateError::WorldIdentityMismatch {
                    expected: expected_world,
                    actual: frame.resulting_world,
                });
            }

            world.id = frame.resulting_world;
            world.version = frame.resulting_version;
            world.next_entity = frame.resulting_allocator;
            world.operation_count += frame.operations.len();
            world.history = Some(Arc::new(HistoryNode {
                parent: world.history.clone(),
                frame: frame.clone(),
            }));
        }
        Ok(world)
    }
}

pub struct CandidateWorld {
    base_world: WorldId,
    base_version: u64,
    id: WorldId,
    version: u64,
    next_entity: u64,
    base_operation_count: usize,
    operations: Arc<[Operation]>,
    kernel: ForthDb,
}

impl fmt::Debug for CandidateWorld {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CandidateWorld")
            .field("base_world", &self.base_world)
            .field("base_version", &self.base_version)
            .field("id", &self.id)
            .field("version", &self.version)
            .field("next_entity", &self.next_entity)
            .field("operations", &self.operations.len())
            .field("active_slots", &self.kernel.active_slot_count())
            .field("records", &self.kernel.record_count())
            .finish()
    }
}

impl CandidateWorld {
    fn construct(base: &World, operations: Vec<Operation>) -> Result<Self, CandidateError> {
        let base_frames = base.frames();
        let reconstructed = World::reconstruct(&base_frames)?;
        if reconstructed.id != base.id || reconstructed.version != base.version {
            return Err(CandidateError::BaseWorldMismatch {
                expected: base.id,
                actual: reconstructed.id,
            });
        }

        let mut kernel = reconstructed.kernel;
        let mut next_entity = reconstructed.next_entity;
        for operation in &operations {
            apply_operation(&mut kernel, &mut next_entity, operation)?;
        }
        kernel
            .validate()
            .map_err(CandidateError::KernelInvariant)?;

        let version = base.version + 1;
        let id = calculate_world_id(base.id, version, next_entity, &operations);
        Ok(Self {
            base_world: base.id,
            base_version: base.version,
            id,
            version,
            next_entity,
            base_operation_count: base.operation_count,
            operations: Arc::from(operations),
            kernel,
        })
    }

    pub fn base_world(&self) -> WorldId {
        self.base_world
    }

    pub fn id(&self) -> WorldId {
        self.id
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn next_entity(&self) -> u64 {
        self.next_entity
    }

    pub fn operations(&self) -> &[Operation] {
        &self.operations
    }

    pub fn active_slot_count(&self) -> usize {
        self.kernel.active_slot_count()
    }

    pub fn record_count(&self) -> usize {
        self.kernel.record_count()
    }

    pub fn resolve(&self, slot: &SlotId) -> Option<&Fact> {
        self.kernel.resolve(slot)
    }

    pub fn definitions(&self, slot: &SlotId) -> Vec<&Fact> {
        self.kernel.definitions(slot)
    }

    pub fn query(&self, patterns: &[Pattern], options: QueryOptions) -> QueryResult {
        self.kernel.query(patterns, options)
    }

    fn commit_frame(&self) -> Arc<CommitFrame> {
        Arc::new(CommitFrame {
            parent_world: self.base_world,
            resulting_world: self.id,
            parent_version: self.base_version,
            resulting_version: self.version,
            resulting_allocator: self.next_entity,
            operations: self.operations.clone(),
        })
    }

    fn into_world(
        self,
        frame: Arc<CommitFrame>,
        parent_history: Option<Arc<HistoryNode>>,
    ) -> World {
        World {
            id: self.id,
            version: self.version,
            next_entity: self.next_entity,
            operation_count: self.base_operation_count + self.operations.len(),
            kernel: self.kernel,
            history: Some(Arc::new(HistoryNode {
                parent: parent_history,
                frame,
            })),
        }
    }
}

pub struct Transaction {
    base: Arc<World>,
    next_entity: u64,
    operations: Vec<Operation>,
}

impl Transaction {
    fn new(base: Arc<World>) -> Self {
        Self {
            next_entity: base.next_entity,
            base,
            operations: Vec::new(),
        }
    }

    pub fn base_world(&self) -> WorldId {
        self.base.id
    }

    pub fn operation_count(&self) -> usize {
        self.operations.len()
    }

    pub fn entity(&mut self) -> EntityId {
        let entity = EntityId::new(self.next_entity);
        self.next_entity += 1;
        self.operations.push(Operation::AllocateEntity { entity });
        entity
    }

    pub fn define(&mut self, slot: SlotId, fact: Fact) {
        self.operations.push(Operation::Define { slot, fact });
    }

    pub fn forget(&mut self, slot: SlotId) {
        self.operations.push(Operation::Forget { slot });
    }

    pub fn define_display_name(&mut self, entity: EntityId, name: impl Into<String>) {
        self.define(
            ForthDb::display_slot(entity),
            Fact::new(
                Atom::Entity(entity),
                Predicate::new("display_name"),
                Atom::Literal(Literal::new(name)),
            ),
        );
    }

    pub fn bind_symbol(&mut self, namespace: &str, symbol: Symbol, entity: EntityId) {
        self.define(
            ForthDb::symbol_slot(namespace, &symbol),
            Fact::new(
                Atom::Literal(Literal::new(format!(
                    "{namespace}:{}",
                    symbol.as_str()
                ))),
                Predicate::new("resolves_to"),
                Atom::Entity(entity),
            ),
        );
    }

    pub fn candidate(&self) -> Result<CandidateWorld, CandidateError> {
        CandidateWorld::construct(&self.base, self.operations.clone())
    }
}

pub type Validator = Arc<dyn Fn(&CandidateWorld) -> Result<(), String> + Send + Sync>;

pub struct Database<S: CommitStore> {
    current: RwLock<Arc<World>>,
    store: Mutex<S>,
    commit_lock: Mutex<()>,
    validators: RwLock<Vec<Validator>>,
}

impl<S: CommitStore> Database<S> {
    pub fn new(store: S) -> Result<Self, CandidateError> {
        let world = Arc::new(World::reconstruct(&store.frames())?);
        Ok(Self {
            current: RwLock::new(world),
            store: Mutex::new(store),
            commit_lock: Mutex::new(()),
            validators: RwLock::new(Vec::new()),
        })
    }

    pub fn snapshot(&self) -> Arc<World> {
        self.current
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn begin(&self) -> Transaction {
        Transaction::new(self.snapshot())
    }

    pub fn register_validator<F>(&self, validator: F)
    where
        F: Fn(&CandidateWorld) -> Result<(), String> + Send + Sync + 'static,
    {
        self.validators
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Arc::new(validator));
    }

    pub fn frame_count(&self) -> usize {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub fn frames(&self) -> Vec<Arc<CommitFrame>> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .frames()
    }

    pub fn commit(&self, transaction: Transaction) -> Result<Arc<World>, CommitError<S::Error>> {
        let _commit_guard = self
            .commit_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = self.snapshot();
        if transaction.base.id != current.id {
            return Err(CommitError::StaleTransaction {
                based_on: transaction.base.id,
                current: current.id,
            });
        }

        let candidate = transaction.candidate().map_err(CommitError::Candidate)?;
        let validators = self
            .validators
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for validator in validators.iter() {
            validator(&candidate).map_err(CommitError::Validation)?;
        }
        drop(validators);

        let frame = candidate.commit_frame();
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .append(frame.clone())
            .map_err(CommitError::Store)?;

        let world = Arc::new(candidate.into_world(frame, current.history.clone()));
        *self
            .current
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = world.clone();
        Ok(world)
    }
}

#[derive(Debug)]
pub enum CandidateError {
    HistoryParentMismatch { expected: WorldId, actual: WorldId },
    HistoryVersionMismatch { expected: u64, actual: u64 },
    AllocatorOperationMismatch { expected: u64, actual: u64 },
    AllocatorStateMismatch { expected: u64, actual: u64 },
    WorldIdentityMismatch { expected: WorldId, actual: WorldId },
    BaseWorldMismatch { expected: WorldId, actual: WorldId },
    KernelInvariant(String),
}

impl fmt::Display for CandidateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HistoryParentMismatch { expected, actual } => write!(
                formatter,
                "commit history expected parent {expected}, found {actual}"
            ),
            Self::HistoryVersionMismatch { expected, actual } => write!(
                formatter,
                "commit history expected parent version {expected}, found {actual}"
            ),
            Self::AllocatorOperationMismatch { expected, actual } => write!(
                formatter,
                "entity allocation expected identifier {expected}, found {actual}"
            ),
            Self::AllocatorStateMismatch { expected, actual } => write!(
                formatter,
                "commit frame expected allocator state {expected}, reconstructed {actual}"
            ),
            Self::WorldIdentityMismatch { expected, actual } => write!(
                formatter,
                "commit frame expected resulting identity {expected}, found {actual}"
            ),
            Self::BaseWorldMismatch { expected, actual } => write!(
                formatter,
                "candidate expected base world {expected}, reconstructed {actual}"
            ),
            Self::KernelInvariant(message) => {
                write!(formatter, "candidate violates kernel invariants: {message}")
            }
        }
    }
}

impl Error for CandidateError {}

#[derive(Debug)]
pub enum CommitError<E: Error + 'static> {
    StaleTransaction { based_on: WorldId, current: WorldId },
    Candidate(CandidateError),
    Validation(String),
    Store(E),
}

impl<E: Error + 'static> fmt::Display for CommitError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleTransaction { based_on, current } => write!(
                formatter,
                "transaction is based on stale world {based_on}; current world is {current}"
            ),
            Self::Candidate(error) => write!(formatter, "candidate construction failed: {error}"),
            Self::Validation(message) => write!(formatter, "candidate validation failed: {message}"),
            Self::Store(error) => write!(formatter, "commit store append failed: {error}"),
        }
    }
}

impl<E: Error + 'static> Error for CommitError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Candidate(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::StaleTransaction { .. } | Self::Validation(_) => None,
        }
    }
}

fn apply_operation(
    kernel: &mut ForthDb,
    next_entity: &mut u64,
    operation: &Operation,
) -> Result<(), CandidateError> {
    match operation {
        Operation::AllocateEntity { entity } => {
            if entity.value() != *next_entity {
                return Err(CandidateError::AllocatorOperationMismatch {
                    expected: *next_entity,
                    actual: entity.value(),
                });
            }
            let actual = kernel.entity();
            if actual != *entity {
                return Err(CandidateError::AllocatorOperationMismatch {
                    expected: entity.value(),
                    actual: actual.value(),
                });
            }
            *next_entity += 1;
        }
        Operation::Define { slot, fact } => {
            kernel.define(slot.clone(), fact.clone());
        }
        Operation::Forget { slot } => {
            kernel.forget(slot.clone());
        }
    }
    Ok(())
}

fn calculate_world_id(
    parent: WorldId,
    resulting_version: u64,
    resulting_allocator: u64,
    operations: &[Operation],
) -> WorldId {
    let mut hasher = StableHasher::new();
    hasher.u64(parent.value());
    hasher.u64(resulting_version);
    hasher.u64(resulting_allocator);
    hasher.u64(operations.len() as u64);
    for operation in operations {
        match operation {
            Operation::AllocateEntity { entity } => {
                hasher.byte(0);
                hasher.u64(entity.value());
            }
            Operation::Define { slot, fact } => {
                hasher.byte(1);
                hasher.string(slot.as_str());
                hash_fact(&mut hasher, fact);
            }
            Operation::Forget { slot } => {
                hasher.byte(2);
                hasher.string(slot.as_str());
            }
        }
    }
    WorldId::new(hasher.finish())
}

fn hash_fact(hasher: &mut StableHasher, fact: &Fact) {
    hash_atom(hasher, &fact.subject);
    hasher.string(fact.predicate.as_str());
    hash_atom(hasher, &fact.object);
}

fn hash_atom(hasher: &mut StableHasher, atom: &Atom) {
    match atom {
        Atom::Entity(entity) => {
            hasher.byte(0);
            hasher.u64(entity.value());
        }
        Atom::Literal(literal) => {
            hasher.byte(1);
            hasher.string(literal.as_str());
        }
    }
}

struct StableHasher(u64);

impl StableHasher {
    fn new() -> Self {
        Self(FNV_OFFSET_BASIS)
    }

    fn byte(&mut self, byte: u8) {
        self.0 ^= u64::from(byte);
        self.0 = self.0.wrapping_mul(FNV_PRIME);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.byte(*byte);
        }
    }

    fn u64(&mut self, value: u64) {
        self.bytes(&value.to_le_bytes());
    }

    fn string(&mut self, value: &str) {
        self.u64(value.len() as u64);
        self.bytes(value.as_bytes());
    }

    fn finish(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_fact(entity: EntityId, value: &str) -> Fact {
        Fact::new(
            Atom::Entity(entity),
            Predicate::new("state"),
            Atom::Literal(Literal::new(value)),
        )
    }

    #[test]
    fn commit_publishes_complete_successor_and_preserves_old_snapshot() {
        let database = Database::new(MemoryCommitStore::new()).expect("empty store is valid");
        let old = database.snapshot();
        let mut transaction = database.begin();
        let entity = transaction.entity();
        let state = SlotId::new("service/state");
        let owner = SlotId::new("service/owner");
        transaction.define(state.clone(), state_fact(entity, "ready"));
        transaction.define(
            owner.clone(),
            Fact::new(
                Atom::Entity(entity),
                Predicate::new("owner"),
                Atom::Literal(Literal::new("operations")),
            ),
        );

        let candidate = transaction.candidate().expect("candidate should build");
        assert!(candidate.resolve(&state).is_some());
        assert!(candidate.resolve(&owner).is_some());
        assert!(old.resolve(&state).is_none());
        assert!(old.resolve(&owner).is_none());

        let current = database.commit(transaction).expect("commit should publish");
        assert_eq!(current.version(), 1);
        assert!(current.resolve(&state).is_some());
        assert!(current.resolve(&owner).is_some());
        assert!(old.resolve(&state).is_none());
        assert_eq!(database.frame_count(), 1);
    }

    #[test]
    fn validator_rejection_appends_nothing_and_publishes_nothing() {
        let database = Database::new(MemoryCommitStore::new()).expect("empty store is valid");
        let required = SlotId::new("release/approval");
        let required_for_validator = required.clone();
        database.register_validator(move |candidate| {
            candidate
                .resolve(&required_for_validator)
                .map(|_| ())
                .ok_or_else(|| "release requires approval".to_owned())
        });

        let transaction = database.begin();
        let error = database
            .commit(transaction)
            .expect_err("validator should reject missing approval");
        assert!(matches!(error, CommitError::Validation(_)));
        assert_eq!(database.snapshot().version(), 0);
        assert_eq!(database.frame_count(), 0);

        let mut approved = database.begin();
        approved.define(
            required.clone(),
            Fact::new(
                Atom::Literal(Literal::new("release")),
                Predicate::new("approved_by"),
                Atom::Literal(Literal::new("operator")),
            ),
        );
        database.commit(approved).expect("approved release commits");
        assert_eq!(database.snapshot().version(), 1);
        assert_eq!(database.frame_count(), 1);
    }

    #[test]
    fn stale_writer_aborts_without_extending_history() {
        let database = Database::new(MemoryCommitStore::new()).expect("empty store is valid");
        let mut first = database.begin();
        let mut stale = database.begin();
        let first_entity = first.entity();
        let stale_entity = stale.entity();
        first.define(
            SlotId::new("winner/state"),
            state_fact(first_entity, "committed"),
        );
        stale.define(
            SlotId::new("stale/state"),
            state_fact(stale_entity, "discarded"),
        );

        database.commit(first).expect("first writer commits");
        let error = database
            .commit(stale)
            .expect_err("stale writer must abort");
        assert!(matches!(error, CommitError::StaleTransaction { .. }));
        assert_eq!(database.snapshot().version(), 1);
        assert_eq!(database.frame_count(), 1);
    }

    #[test]
    fn define_and_forget_preserve_historical_truth() {
        let database = Database::new(MemoryCommitStore::new()).expect("empty store is valid");
        let slot = SlotId::new("service/state");

        let mut initial = database.begin();
        let entity = initial.entity();
        initial.define(slot.clone(), state_fact(entity, "v1"));
        database.commit(initial).expect("initial world commits");

        let mut update = database.begin();
        update.define(slot.clone(), state_fact(entity, "v2"));
        update.forget(slot.clone());
        let candidate = update.candidate().expect("candidate should read staged writes");
        assert_eq!(
            candidate.resolve(&slot).map(|fact| &fact.object),
            Some(&Atom::Literal(Literal::new("v1")))
        );
        let world = database.commit(update).expect("forget world commits");
        assert_eq!(world.definitions(&slot).len(), 1);
        assert_eq!(world.record_count(), 3);
    }

    #[test]
    fn identical_histories_produce_identical_frames_and_world_ids() {
        fn run() -> (WorldId, Vec<Arc<CommitFrame>>) {
            let database =
                Database::new(MemoryCommitStore::new()).expect("empty store is valid");
            let mut transaction = database.begin();
            let entity = transaction.entity();
            transaction.define(
                SlotId::new("deterministic/state"),
                state_fact(entity, "ready"),
            );
            let world = database.commit(transaction).expect("commit succeeds");
            (world.id(), database.frames())
        }

        let first = run();
        let second = run();
        assert_eq!(first.0, second.0);
        assert_eq!(first.1, second.1);
    }

    #[test]
    fn a_database_reconstructs_the_latest_world_from_memory_store_frames() {
        let database = Database::new(MemoryCommitStore::new()).expect("empty store is valid");
        let mut transaction = database.begin();
        let entity = transaction.entity();
        let slot = SlotId::new("reconstructed/state");
        transaction.define(slot.clone(), state_fact(entity, "ready"));
        let committed = database.commit(transaction).expect("commit succeeds");

        let mut store = MemoryCommitStore::new();
        for frame in database.frames() {
            store.append(frame).expect("memory append is infallible");
        }
        let reopened = Database::new(store).expect("memory history reconstructs");
        assert_eq!(reopened.snapshot().id(), committed.id());
        assert_eq!(reopened.snapshot().version(), committed.version());
        assert!(reopened.snapshot().resolve(&slot).is_some());
    }
}
