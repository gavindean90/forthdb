use super::*;
use std::collections::BTreeMap;

/// An entity identifier local to exactly one queued intent.
///
/// Temporary identifiers are resolved only when the committer assigns the
/// intent a private predecessor world. The same numeric temporary identifier
/// in two intents therefore never aliases.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TempEntity(u32);

impl TempEntity {
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// An atom in a queued intent. Unlike a committed [`Atom`], it may refer to an
/// entity that has not been assigned a durable identifier yet.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum IntentAtom {
    Entity(EntityId),
    Temporary(TempEntity),
    Literal(Literal),
}

impl From<EntityId> for IntentAtom {
    fn from(value: EntityId) -> Self {
        Self::Entity(value)
    }
}

impl From<TempEntity> for IntentAtom {
    fn from(value: TempEntity) -> Self {
        Self::Temporary(value)
    }
}

impl From<Literal> for IntentAtom {
    fn from(value: Literal) -> Self {
        Self::Literal(value)
    }
}

impl From<Atom> for IntentAtom {
    fn from(value: Atom) -> Self {
        match value {
            Atom::Entity(entity) => Self::Entity(entity),
            Atom::Literal(literal) => Self::Literal(literal),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct IntentFact {
    pub subject: IntentAtom,
    pub predicate: Predicate,
    pub object: IntentAtom,
}

impl IntentFact {
    pub fn new(
        subject: impl Into<IntentAtom>,
        predicate: Predicate,
        object: impl Into<IntentAtom>,
    ) -> Self {
        Self {
            subject: subject.into(),
            predicate,
            object: object.into(),
        }
    }
}

impl From<Fact> for IntentFact {
    fn from(value: Fact) -> Self {
        Self {
            subject: value.subject.into(),
            predicate: value.predicate,
            object: value.object.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IntentPrecondition {
    ExpectedWorld(WorldId),
    ExpectedSlot {
        slot: SlotId,
        expected: Option<Fact>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum IntentOperation {
    AllocateEntity { temporary: TempEntity },
    Define { slot: SlotId, fact: IntentFact },
    Forget { slot: SlotId },
}

/// Operations that explicitly delegate predecessor assignment to the epoch
/// planner. This is intentionally distinct from [`Transaction`], whose base
/// world remains absolute and whose stale-writer behavior is unchanged.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct QueuedIntent {
    next_temporary: u32,
    preconditions: Vec<IntentPrecondition>,
    operations: Vec<IntentOperation>,
}

impl QueuedIntent {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn operation_count(&self) -> usize {
        self.operations.len()
    }

    pub fn precondition_count(&self) -> usize {
        self.preconditions.len()
    }

    pub fn expect_world(&mut self, world: WorldId) {
        self.preconditions
            .push(IntentPrecondition::ExpectedWorld(world));
    }

    pub fn expect_value(&mut self, slot: SlotId, fact: Fact) {
        self.preconditions.push(IntentPrecondition::ExpectedSlot {
            slot,
            expected: Some(fact),
        });
    }

    pub fn expect_absent(&mut self, slot: SlotId) {
        self.preconditions.push(IntentPrecondition::ExpectedSlot {
            slot,
            expected: None,
        });
    }

    pub fn entity(&mut self) -> TempEntity {
        let temporary = TempEntity(self.next_temporary);
        self.next_temporary = self
            .next_temporary
            .checked_add(1)
            .expect("queued intent temporary-entity identifier overflow");
        self.operations
            .push(IntentOperation::AllocateEntity { temporary });
        temporary
    }

    pub fn define(&mut self, slot: SlotId, fact: IntentFact) {
        self.operations.push(IntentOperation::Define { slot, fact });
    }

    pub fn define_fact(&mut self, slot: SlotId, fact: Fact) {
        self.define(slot, fact.into());
    }

    pub fn forget(&mut self, slot: SlotId) {
        self.operations.push(IntentOperation::Forget { slot });
    }
}

#[derive(Debug)]
pub enum IntentRejection {
    WorldPrecondition {
        expected: WorldId,
        actual: WorldId,
    },
    SlotPrecondition {
        slot: SlotId,
        expected: Option<Fact>,
        actual: Option<Fact>,
    },
    UnknownTemporaryEntity(TempEntity),
    Candidate(CandidateError),
    Validation(String),
}

impl fmt::Display for IntentRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorldPrecondition { expected, actual } => write!(
                formatter,
                "queued intent expected predecessor {expected}, found {actual}"
            ),
            Self::SlotPrecondition {
                slot,
                expected,
                actual,
            } => write!(
                formatter,
                "queued intent slot precondition failed for {slot:?}: expected {expected:?}, found {actual:?}"
            ),
            Self::UnknownTemporaryEntity(entity) => write!(
                formatter,
                "queued intent referenced unallocated temporary entity {}",
                entity.index()
            ),
            Self::Candidate(error) => write!(formatter, "queued candidate failed: {error}"),
            Self::Validation(message) => {
                write!(formatter, "queued candidate validation failed: {message}")
            }
        }
    }
}

impl Error for IntentRejection {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Candidate(error) => Some(error),
            Self::WorldPrecondition { .. }
            | Self::SlotPrecondition { .. }
            | Self::UnknownTemporaryEntity(_)
            | Self::Validation(_) => None,
        }
    }
}

#[derive(Debug)]
pub struct AcceptedIntent {
    position: usize,
    world: Arc<World>,
    frame: Arc<CommitFrame>,
    entities: BTreeMap<TempEntity, EntityId>,
}

impl AcceptedIntent {
    pub fn position(&self) -> usize {
        self.position
    }

    pub fn world(&self) -> Arc<World> {
        self.world.clone()
    }

    pub fn frame(&self) -> Arc<CommitFrame> {
        self.frame.clone()
    }

    pub fn entity(&self, temporary: TempEntity) -> Option<EntityId> {
        self.entities.get(&temporary).copied()
    }

    pub fn entities(&self) -> &BTreeMap<TempEntity, EntityId> {
        &self.entities
    }
}

#[derive(Debug)]
pub struct RejectedIntent {
    position: usize,
    error: IntentRejection,
}

impl RejectedIntent {
    pub fn position(&self) -> usize {
        self.position
    }

    pub fn error(&self) -> &IntentRejection {
        &self.error
    }
}

#[derive(Debug)]
pub enum EpochOutcome {
    Accepted(AcceptedIntent),
    Rejected(RejectedIntent),
}

impl EpochOutcome {
    pub fn position(&self) -> usize {
        match self {
            Self::Accepted(accepted) => accepted.position(),
            Self::Rejected(rejected) => rejected.position(),
        }
    }

    pub fn accepted(&self) -> Option<&AcceptedIntent> {
        match self {
            Self::Accepted(accepted) => Some(accepted),
            Self::Rejected(_) => None,
        }
    }

    pub fn rejected(&self) -> Option<&RejectedIntent> {
        match self {
            Self::Accepted(_) => None,
            Self::Rejected(rejected) => Some(rejected),
        }
    }
}

/// A deterministic private successor chain. Constructing a plan never mutates
/// the supplied base world and never publishes a successor.
#[derive(Debug)]
pub struct EpochPlan {
    base: Arc<World>,
    tail: Arc<World>,
    outcomes: Vec<EpochOutcome>,
    frames: Vec<Arc<CommitFrame>>,
}

impl EpochPlan {
    pub fn base(&self) -> Arc<World> {
        self.base.clone()
    }

    pub fn tail(&self) -> Arc<World> {
        self.tail.clone()
    }

    pub fn outcomes(&self) -> &[EpochOutcome] {
        &self.outcomes
    }

    pub fn frames(&self) -> &[Arc<CommitFrame>] {
        &self.frames
    }

    pub fn accepted_count(&self) -> usize {
        self.frames.len()
    }

    pub fn rejected_count(&self) -> usize {
        self.outcomes.len() - self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

/// Derive a private chain in ingress order.
///
/// Preconditions and validators are evaluated against the predecessor assigned
/// to each intent. Rejected intents consume neither a world version nor an
/// entity identifier, and the following intent continues from the previous
/// accepted world.
pub fn derive_epoch(
    base: Arc<World>,
    intents: Vec<QueuedIntent>,
    validators: &[Validator],
) -> EpochPlan {
    let mut predecessor = base.clone();
    let mut outcomes = Vec::with_capacity(intents.len());
    let mut frames = Vec::with_capacity(intents.len());

    for (position, intent) in intents.into_iter().enumerate() {
        let result = derive_intent(predecessor.clone(), intent, validators);
        match result {
            Ok((world, frame, entities)) => {
                predecessor = world.clone();
                frames.push(frame.clone());
                outcomes.push(EpochOutcome::Accepted(AcceptedIntent {
                    position,
                    world,
                    frame,
                    entities,
                }));
            }
            Err(error) => outcomes.push(EpochOutcome::Rejected(RejectedIntent {
                position,
                error,
            })),
        }
    }

    EpochPlan {
        base,
        tail: predecessor,
        outcomes,
        frames,
    }
}

fn derive_intent(
    predecessor: Arc<World>,
    intent: QueuedIntent,
    validators: &[Validator],
) -> Result<
    (
        Arc<World>,
        Arc<CommitFrame>,
        BTreeMap<TempEntity, EntityId>,
    ),
    IntentRejection,
> {
    check_preconditions(&predecessor, &intent.preconditions)?;
    let (operations, entities) = resolve_operations(&predecessor, intent.operations)?;
    let candidate =
        CandidateWorld::construct(predecessor.as_ref(), operations).map_err(IntentRejection::Candidate)?;
    for validator in validators {
        validator(&candidate).map_err(IntentRejection::Validation)?;
    }
    let frame = candidate.commit_frame();
    let world = Arc::new(candidate.into_world(frame.clone(), predecessor.history.clone()));
    Ok((world, frame, entities))
}

fn check_preconditions(
    predecessor: &World,
    preconditions: &[IntentPrecondition],
) -> Result<(), IntentRejection> {
    for precondition in preconditions {
        match precondition {
            IntentPrecondition::ExpectedWorld(expected) if *expected != predecessor.id() => {
                return Err(IntentRejection::WorldPrecondition {
                    expected: *expected,
                    actual: predecessor.id(),
                });
            }
            IntentPrecondition::ExpectedWorld(_) => {}
            IntentPrecondition::ExpectedSlot { slot, expected } => {
                let actual = predecessor.resolve(slot).cloned();
                if actual != *expected {
                    return Err(IntentRejection::SlotPrecondition {
                        slot: slot.clone(),
                        expected: expected.clone(),
                        actual,
                    });
                }
            }
        }
    }
    Ok(())
}

fn resolve_operations(
    predecessor: &World,
    intent_operations: Vec<IntentOperation>,
) -> Result<(Vec<Operation>, BTreeMap<TempEntity, EntityId>), IntentRejection> {
    let mut next_entity = predecessor.next_entity();
    let mut entities = BTreeMap::new();
    let mut operations = Vec::with_capacity(intent_operations.len());

    for operation in intent_operations {
        match operation {
            IntentOperation::AllocateEntity { temporary } => {
                let entity = EntityId::new(next_entity);
                next_entity = next_entity
                    .checked_add(1)
                    .expect("world entity allocator overflow");
                entities.insert(temporary, entity);
                operations.push(Operation::AllocateEntity { entity });
            }
            IntentOperation::Define { slot, fact } => {
                operations.push(Operation::Define {
                    slot,
                    fact: resolve_fact(fact, &entities)?,
                });
            }
            IntentOperation::Forget { slot } => operations.push(Operation::Forget { slot }),
        }
    }

    Ok((operations, entities))
}

fn resolve_fact(
    fact: IntentFact,
    entities: &BTreeMap<TempEntity, EntityId>,
) -> Result<Fact, IntentRejection> {
    Ok(Fact::new(
        resolve_atom(fact.subject, entities)?,
        fact.predicate,
        resolve_atom(fact.object, entities)?,
    ))
}

fn resolve_atom(
    atom: IntentAtom,
    entities: &BTreeMap<TempEntity, EntityId>,
) -> Result<Atom, IntentRejection> {
    match atom {
        IntentAtom::Entity(entity) => Ok(Atom::Entity(entity)),
        IntentAtom::Temporary(temporary) => entities
            .get(&temporary)
            .copied()
            .map(Atom::Entity)
            .ok_or(IntentRejection::UnknownTemporaryEntity(temporary)),
        IntentAtom::Literal(literal) => Ok(Atom::Literal(literal)),
    }
}

/// Stage 6A's in-memory publication control. All accepted frames are appended
/// to the infallible memory store, then the global reader head is advanced once
/// to the epoch tail. Strict commits and queued epochs share `commit_lock`.
impl Database<MemoryCommitStore> {
    pub fn commit_queued_epoch(&self, intents: Vec<QueuedIntent>) -> EpochPlan {
        let _commit_guard = self
            .commit_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let base = self.snapshot();
        let validators = self
            .validators
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let plan = derive_epoch(base, intents, &validators);

        if !plan.is_empty() {
            let mut store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for frame in plan.frames() {
                match store.append(frame.clone()) {
                    Ok(()) => {}
                    Err(never) => match never {},
                }
            }
            drop(store);

            *self
                .current
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = plan.tail();
        }

        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_store::FileCommitStore;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn state_fact(entity: EntityId, value: &str) -> Fact {
        Fact::new(
            Atom::Entity(entity),
            Predicate::new("state"),
            Atom::Literal(Literal::new(value)),
        )
    }

    fn intent_state_fact(entity: impl Into<IntentAtom>, value: &str) -> IntentFact {
        IntentFact::new(
            entity,
            Predicate::new("state"),
            Literal::new(value),
        )
    }

    fn replay_frame(database: &Database<MemoryCommitStore>, frame: &CommitFrame) -> Arc<World> {
        let mut transaction = database.begin();
        for operation in frame.operations() {
            match operation {
                Operation::AllocateEntity { entity } => {
                    assert_eq!(transaction.entity(), *entity);
                }
                Operation::Define { slot, fact } => {
                    transaction.define(slot.clone(), fact.clone());
                }
                Operation::Forget { slot } => transaction.forget(slot.clone()),
            }
        }
        database.commit(transaction).expect("oracle commit succeeds")
    }

    #[test]
    fn temporary_entities_are_scoped_and_resolved_from_each_predecessor() {
        let base = Arc::new(World::genesis());
        let mut first = QueuedIntent::new();
        let first_temp = first.entity();
        first.define(
            SlotId::new("first/state"),
            intent_state_fact(first_temp, "ready"),
        );

        let mut second = QueuedIntent::new();
        let second_temp = second.entity();
        second.define(
            SlotId::new("second/state"),
            intent_state_fact(second_temp, "ready"),
        );

        assert_eq!(first_temp, second_temp, "temporary namespaces may reuse indexes");
        let plan = derive_epoch(base, vec![first, second], &[]);
        let first = plan.outcomes()[0].accepted().expect("first accepted");
        let second = plan.outcomes()[1].accepted().expect("second accepted");
        assert_eq!(first.entity(first_temp), Some(EntityId::new(1)));
        assert_eq!(second.entity(second_temp), Some(EntityId::new(2)));
        assert_eq!(plan.tail().next_entity(), 3);
    }

    #[test]
    fn a_rejected_intent_consumes_neither_version_nor_allocator_state() {
        let base = Arc::new(World::genesis());

        let mut accepted_first = QueuedIntent::new();
        let first_entity = accepted_first.entity();
        accepted_first.define(
            SlotId::new("first/state"),
            intent_state_fact(first_entity, "ready"),
        );

        let mut rejected = QueuedIntent::new();
        rejected.expect_world(WorldId::new(7));
        let rejected_entity = rejected.entity();
        rejected.define(
            SlotId::new("rejected/state"),
            intent_state_fact(rejected_entity, "never"),
        );

        let mut accepted_last = QueuedIntent::new();
        let last_entity = accepted_last.entity();
        accepted_last.define(
            SlotId::new("last/state"),
            intent_state_fact(last_entity, "ready"),
        );

        let plan = derive_epoch(base, vec![accepted_first, rejected, accepted_last], &[]);
        assert_eq!(plan.accepted_count(), 2);
        assert_eq!(plan.rejected_count(), 1);
        assert_eq!(plan.tail().version(), 2);
        assert_eq!(plan.tail().next_entity(), 3);
        let last = plan.outcomes()[2].accepted().expect("last accepted");
        assert_eq!(last.entity(last_entity), Some(EntityId::new(2)));
        assert!(plan.tail().resolve(&SlotId::new("rejected/state")).is_none());
    }

    #[test]
    fn slot_preconditions_observe_the_assigned_private_predecessor() {
        let slot = SlotId::new("service/state");
        let initial_entity = EntityId::new(1);
        let initial_fact = state_fact(initial_entity, "one");
        let updated_fact = state_fact(initial_entity, "two");

        let database = Database::new(MemoryCommitStore::new()).expect("genesis valid");
        let mut setup = database.begin();
        assert_eq!(setup.entity(), initial_entity);
        setup.define(slot.clone(), initial_fact.clone());
        let base = database.commit(setup).expect("setup commits");

        let mut first = QueuedIntent::new();
        first.expect_value(slot.clone(), initial_fact.clone());
        first.define_fact(slot.clone(), updated_fact.clone());

        let mut stale_expectation = QueuedIntent::new();
        stale_expectation.expect_value(slot.clone(), initial_fact);
        stale_expectation.define_fact(
            SlotId::new("should/not/exist"),
            Fact::new(
                Atom::Literal(Literal::new("x")),
                Predicate::new("value"),
                Atom::Literal(Literal::new("y")),
            ),
        );

        let mut current_expectation = QueuedIntent::new();
        current_expectation.expect_value(slot.clone(), updated_fact.clone());
        current_expectation.define_fact(
            SlotId::new("should/exist"),
            Fact::new(
                Atom::Literal(Literal::new("x")),
                Predicate::new("value"),
                Atom::Literal(Literal::new("z")),
            ),
        );

        let plan = derive_epoch(
            base,
            vec![first, stale_expectation, current_expectation],
            &[],
        );
        assert!(plan.outcomes()[0].accepted().is_some());
        assert!(matches!(
            plan.outcomes()[1].rejected().expect("rejected").error(),
            IntentRejection::SlotPrecondition { .. }
        ));
        assert!(plan.outcomes()[2].accepted().is_some());
        assert_eq!(plan.tail().resolve(&slot), Some(&updated_fact));
        assert!(plan.tail().resolve(&SlotId::new("should/not/exist")).is_none());
        assert!(plan.tail().resolve(&SlotId::new("should/exist")).is_some());
    }

    #[test]
    fn validators_reject_only_the_assigned_intent() {
        let required = SlotId::new("approval");
        let validator_slot = required.clone();
        let validator: Validator = Arc::new(move |candidate| {
            candidate
                .resolve(&validator_slot)
                .map(|_| ())
                .ok_or_else(|| "approval required".to_owned())
        });

        let mut rejected = QueuedIntent::new();
        rejected.define_fact(
            SlotId::new("work/one"),
            Fact::new(
                Atom::Literal(Literal::new("one")),
                Predicate::new("state"),
                Atom::Literal(Literal::new("ready")),
            ),
        );

        let mut accepted = QueuedIntent::new();
        accepted.define_fact(
            required.clone(),
            Fact::new(
                Atom::Literal(Literal::new("release")),
                Predicate::new("approved_by"),
                Atom::Literal(Literal::new("operator")),
            ),
        );

        let plan = derive_epoch(
            Arc::new(World::genesis()),
            vec![rejected, accepted],
            &[validator],
        );
        assert!(matches!(
            plan.outcomes()[0].rejected().expect("rejected").error(),
            IntentRejection::Validation(_)
        ));
        assert!(plan.outcomes()[1].accepted().is_some());
        assert_eq!(plan.tail().version(), 1);
    }

    #[test]
    fn queued_plan_matches_strict_sequential_worlds_and_frames() {
        let mut intents = Vec::new();
        for index in 0..128 {
            let mut intent = QueuedIntent::new();
            let entity = intent.entity();
            intent.define(
                SlotId::new(format!("service/{index}/state")),
                intent_state_fact(entity, &format!("v{index}")),
            );
            if index % 7 == 0 {
                intent.forget(SlotId::new(format!("service/{index}/state")));
            }
            intents.push(intent);
        }

        let plan = derive_epoch(Arc::new(World::genesis()), intents, &[]);
        let oracle = Database::new(MemoryCommitStore::new()).expect("genesis valid");
        let mut oracle_worlds = Vec::new();
        for frame in plan.frames() {
            oracle_worlds.push(replay_frame(&oracle, frame));
        }

        assert_eq!(oracle.frames(), plan.frames());
        assert_eq!(oracle.snapshot().id(), plan.tail().id());
        assert_eq!(oracle.snapshot().version(), plan.tail().version());
        for (outcome, oracle_world) in plan
            .outcomes()
            .iter()
            .filter_map(EpochOutcome::accepted)
            .zip(oracle_worlds)
        {
            assert_eq!(outcome.world().id(), oracle_world.id());
            assert_eq!(outcome.world().next_entity(), oracle_world.next_entity());
        }
    }

    #[test]
    fn in_memory_epoch_appends_all_frames_then_publishes_only_the_tail() {
        let database = Database::new(MemoryCommitStore::new()).expect("genesis valid");
        let old = database.snapshot();
        let stale = database.begin();
        let mut intents = Vec::new();
        for index in 0..16 {
            let mut intent = QueuedIntent::new();
            intent.define_fact(
                SlotId::new(format!("epoch/{index}")),
                Fact::new(
                    Atom::Literal(Literal::new("epoch")),
                    Predicate::new("value"),
                    Atom::Literal(Literal::new(index.to_string())),
                ),
            );
            intents.push(intent);
        }

        let plan = database.commit_queued_epoch(intents);
        assert_eq!(old.version(), 0);
        assert_eq!(database.snapshot().id(), plan.tail().id());
        assert_eq!(database.snapshot().version(), 16);
        assert_eq!(database.frame_count(), 16);
        assert_eq!(database.frames(), plan.frames());
        assert!(matches!(
            database.commit(stale),
            Err(CommitError::StaleTransaction { .. })
        ));
    }

    #[test]
    fn canonical_file_bytes_match_strict_sequential_execution() {
        let mut intents = Vec::new();
        for index in 0..64 {
            let mut intent = QueuedIntent::new();
            let entity = intent.entity();
            intent.define(
                SlotId::new(format!("canonical/{index}")),
                intent_state_fact(entity, &index.to_string()),
            );
            intents.push(intent);
        }
        let plan = derive_epoch(Arc::new(World::genesis()), intents, &[]);

        let queued_path = temporary_path("queued");
        let strict_path = temporary_path("strict");
        {
            let mut queued_store = FileCommitStore::open(&queued_path).expect("queued file opens");
            for frame in plan.frames() {
                queued_store.append(frame.clone()).expect("frame appends");
            }
        }
        {
            let store = FileCommitStore::open(&strict_path).expect("strict file opens");
            let database = Database::new(store).expect("strict database opens");
            for frame in plan.frames() {
                let mut transaction = database.begin();
                for operation in frame.operations() {
                    match operation {
                        Operation::AllocateEntity { entity } => {
                            assert_eq!(transaction.entity(), *entity);
                        }
                        Operation::Define { slot, fact } => {
                            transaction.define(slot.clone(), fact.clone());
                        }
                        Operation::Forget { slot } => transaction.forget(slot.clone()),
                    }
                }
                database.commit(transaction).expect("strict commit succeeds");
            }
        }

        assert_eq!(fs::read(&queued_path).unwrap(), fs::read(&strict_path).unwrap());
        let _ = fs::remove_file(queued_path);
        let _ = fs::remove_file(strict_path);
    }

    #[test]
    fn deterministic_randomized_epoch_matches_sequential_oracle() {
        let mut seed = 0x6a09_e667_f3bc_c909_u64;
        let mut intents = Vec::with_capacity(10_000);
        for index in 0..10_000_u64 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let slot_index = seed % 257;
            let mut intent = QueuedIntent::new();
            if seed & 7 == 0 {
                intent.forget(SlotId::new(format!("random/{slot_index}")));
            } else {
                intent.define_fact(
                    SlotId::new(format!("random/{slot_index}")),
                    Fact::new(
                        Atom::Literal(Literal::new("random")),
                        Predicate::new("value"),
                        Atom::Literal(Literal::new(format!("{index}:{seed}"))),
                    ),
                );
            }
            intents.push(intent);
        }

        let plan = derive_epoch(Arc::new(World::genesis()), intents, &[]);
        let oracle = Database::new(MemoryCommitStore::new()).expect("genesis valid");
        for frame in plan.frames() {
            replay_frame(&oracle, frame);
        }
        assert_eq!(oracle.frames(), plan.frames());
        assert_eq!(oracle.snapshot().id(), plan.tail().id());
        assert_eq!(oracle.snapshot().record_count(), plan.tail().record_count());
        for slot_index in 0..257 {
            let slot = SlotId::new(format!("random/{slot_index}"));
            assert_eq!(oracle.snapshot().resolve(&slot), plan.tail().resolve(&slot));
        }
    }

    fn temporary_path(label: &str) -> PathBuf {
        let sequence = TEMP_PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "forthdb-m6a-{label}-{}-{sequence}.db",
            std::process::id()
        ))
    }
}
