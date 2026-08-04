use forthdb_core::{
    Atom, Binding, BoundValue, EntityId, Fact, ForthDb, Literal, Pattern, Predicate,
    PredicateTerm, QueryMetrics, QueryOptions, QueryResult, QueryRow, SlotId, Symbol, Term,
};
use crate::mmap_vm_snapshot::MmapVmSnapshot;
use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

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

pub(crate) trait FrameSource: Send + Sync {
    fn frames(&self) -> Vec<Arc<CommitFrame>>;
}

impl HistoryNode {
    fn materialize_kernel(&self, projected: Option<&ProjectionBase>) -> Arc<ForthDb> {
        let projected = projected.map(|base| {
            (base.kernel.clone(), base.version, base.next_entity)
        });
        let projected_version = projected
            .as_ref()
            .map_or(0, |(_, version, _)| *version);
        let mut pending = Vec::new();
        let mut cursor = Some(self);
        while let Some(node) = cursor {
            if node.frame.resulting_version <= projected_version {
                break;
            }
            pending.push(node.frame.clone());
            cursor = node.parent.as_deref();
        }

        let (mut kernel, mut next_entity) = projected
            .map(|(kernel, _, next_entity)| (kernel.as_ref().clone(), next_entity))
            .unwrap_or_else(|| (ForthDb::new(), 1));
        for frame in pending.into_iter().rev() {
            for operation in frame.operations.iter() {
                apply_operation(&mut kernel, &mut next_entity, operation)
                    .expect("published VM operations must materialize deterministically");
            }
            debug_assert_eq!(next_entity, frame.resulting_allocator);
        }
        Arc::new(kernel)
    }
}

#[derive(Clone)]
struct ProjectionBase {
    kernel: Arc<ForthDb>,
    version: u64,
    next_entity: u64,
}

#[derive(Clone)]
struct ProjectedFact {
    record_id: usize,
    slot: SlotId,
    fact: Fact,
}

#[derive(Default)]
struct VmQueryProjection {
    mapped: Option<Arc<MmapVmSnapshot>>,
    definitions: HashMap<SlotId, Vec<ProjectedFact>>,
    active: Vec<ProjectedFact>,
    by_subject: HashMap<Atom, Vec<usize>>,
    by_predicate: HashMap<Predicate, Vec<usize>>,
    by_object: HashMap<Atom, Vec<usize>>,
    by_subject_predicate: HashMap<(Atom, Predicate), Vec<usize>>,
    by_subject_object: HashMap<(Atom, Atom), Vec<usize>>,
    by_predicate_object: HashMap<(Predicate, Atom), Vec<usize>>,
    by_exact: HashMap<Fact, Vec<usize>>,
}

#[derive(Clone)]
struct QueryFrame {
    binding: Binding,
    provenance: Vec<SlotId>,
}

impl VmQueryProjection {
    fn from_mmap(snapshot: Arc<MmapVmSnapshot>) -> Self {
        Self {
            mapped: Some(snapshot),
            ..Self::default()
        }
    }

    fn from_history(history: &HistoryNode) -> Self {
        let mut frames = Vec::new();
        let mut cursor = Some(history);
        while let Some(node) = cursor {
            frames.push(node.frame.clone());
            cursor = node.parent.as_deref();
        }
        frames.reverse();
        Self::from_frames(&frames)
    }

    fn from_frames(frames: &[Arc<CommitFrame>]) -> Self {
        let mut definitions = HashMap::<SlotId, Vec<ProjectedFact>>::new();
        let mut record_id = 0usize;
        for frame in frames {
            for operation in frame.operations.iter() {
                match operation {
                    Operation::AllocateEntity { .. } => {}
                    Operation::Define { slot, fact } => {
                        definitions
                            .entry(slot.clone())
                            .or_default()
                            .push(ProjectedFact {
                                record_id,
                                slot: slot.clone(),
                                fact: fact.clone(),
                            });
                        record_id += 1;
                    }
                    Operation::Forget { slot } => {
                        let remove = if let Some(stack) = definitions.get_mut(slot) {
                            stack.pop();
                            stack.is_empty()
                        } else {
                            false
                        };
                        if remove {
                            definitions.remove(slot);
                        }
                        record_id += 1;
                    }
                }
            }
        }

        let mut active: Vec<_> = definitions
            .values()
            .filter_map(|stack| stack.last().cloned())
            .collect();
        active.sort_unstable_by_key(|entry| entry.record_id);
        let mut projection = Self {
            definitions,
            active,
            ..Self::default()
        };
        for index in 0..projection.active.len() {
            let fact = &projection.active[index].fact;
            projection
                .by_subject
                .entry(fact.subject.clone())
                .or_default()
                .push(index);
            projection
                .by_predicate
                .entry(fact.predicate.clone())
                .or_default()
                .push(index);
            projection
                .by_object
                .entry(fact.object.clone())
                .or_default()
                .push(index);
            projection
                .by_subject_predicate
                .entry((fact.subject.clone(), fact.predicate.clone()))
                .or_default()
                .push(index);
            projection
                .by_subject_object
                .entry((fact.subject.clone(), fact.object.clone()))
                .or_default()
                .push(index);
            projection
                .by_predicate_object
                .entry((fact.predicate.clone(), fact.object.clone()))
                .or_default()
                .push(index);
            projection
                .by_exact
                .entry(fact.clone())
                .or_default()
                .push(index);
        }
        projection
    }

    fn resolve(&self, slot: &SlotId) -> Option<&Fact> {
        if let Some(mapped) = &self.mapped {
            return mapped.resolve(slot);
        }
        self.definitions
            .get(slot)
            .and_then(|stack| stack.last())
            .map(|entry| &entry.fact)
    }

    fn definitions(&self, slot: &SlotId) -> Vec<&Fact> {
        if let Some(mapped) = &self.mapped {
            return mapped.definitions(slot);
        }
        self.definitions
            .get(slot)
            .into_iter()
            .flatten()
            .rev()
            .map(|entry| &entry.fact)
            .collect()
    }

    fn candidates(&self, pattern: &Pattern, binding: &Binding) -> Vec<usize> {
        let subject = resolved_atom(&pattern.subject, binding);
        let predicate = resolved_predicate(&pattern.predicate, binding);
        let object = resolved_atom(&pattern.object, binding);
        let bucket = match (subject, predicate, object) {
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
            (None, None, None) => return (0..self.active.len()).collect(),
        };
        bucket.cloned().unwrap_or_default()
    }

    fn query(&self, patterns: &[Pattern], options: QueryOptions) -> QueryResult {
        if let Some(mapped) = &self.mapped {
            return mapped.query(patterns, options);
        }
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

        QueryResult {
            rows: output
                .into_iter()
                .map(|frame| QueryRow {
                    binding: frame.binding,
                    provenance: if options.include_provenance {
                        frame.provenance
                    } else {
                        Vec::new()
                    },
                })
                .collect(),
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
                .min_by_key(|(_, pattern)| self.candidates(pattern, &frame.binding).len())
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

        let candidates = self.candidates(&chosen, &frame.binding);
        metrics.candidate_facts += candidates.len() as u64;
        for index in candidates {
            let entry = &self.active[index];
            if let Some(binding) = unify_pattern(&chosen, &entry.fact, &frame.binding) {
                metrics.bindings_emitted += 1;
                let mut provenance = frame.provenance.clone();
                provenance.push(entry.slot.clone());
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
}

fn resolved_atom(term: &Term, binding: &Binding) -> Option<Atom> {
    match term {
        Term::Atom(atom) => Some(atom.clone()),
        Term::Variable(variable) => binding
            .get(variable.as_str())
            .and_then(BoundValue::as_atom),
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
    unify_term(
        &pattern.object,
        BoundValue::from(fact.object.clone()),
        &binding,
    )
}

fn unify_term(term: &Term, value: BoundValue, binding: &Binding) -> Option<Binding> {
    match term {
        Term::Atom(atom) if value.as_atom().as_ref() == Some(atom) => Some(binding.clone()),
        Term::Atom(_) => None,
        Term::Variable(variable) => unify_variable(variable.as_str(), value, binding),
    }
}

fn unify_predicate(
    term: &PredicateTerm,
    value: BoundValue,
    binding: &Binding,
) -> Option<Binding> {
    match term {
        PredicateTerm::Predicate(predicate)
            if value.as_predicate().as_ref() == Some(predicate) =>
        {
            Some(binding.clone())
        }
        PredicateTerm::Predicate(_) => None,
        PredicateTerm::Variable(variable) => {
            unify_variable(variable.as_str(), value, binding)
        }
    }
}

fn unify_variable(name: &str, value: BoundValue, binding: &Binding) -> Option<Binding> {
    match binding.get(name) {
        Some(existing) if existing == &value => Some(binding.clone()),
        Some(_) => None,
        None => {
            let mut extended = binding.clone();
            extended.insert(name.to_owned(), value);
            Some(extended)
        }
    }
}

pub struct World {
    id: WorldId,
    version: u64,
    next_entity: u64,
    operation_count: usize,
    active_slot_count: usize,
    record_count: usize,
    eager_kernel: Option<ForthDb>,
    lazy_kernel: OnceLock<Arc<ForthDb>>,
    vm_query: OnceLock<Arc<VmQueryProjection>>,
    projection_base: Option<ProjectionBase>,
    history: Option<Arc<HistoryNode>>,
    history_prefix: Option<Arc<dyn FrameSource>>,
}

impl fmt::Debug for World {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("World")
            .field("id", &self.id)
            .field("version", &self.version)
            .field("next_entity", &self.next_entity)
            .field("operation_count", &self.operation_count)
            .field("active_slots", &self.active_slot_count)
            .field("records", &self.record_count)
            .field(
                "compatibility_materialized",
                &self.is_query_projection_materialized(),
            )
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
            active_slot_count: 0,
            record_count: 0,
            eager_kernel: Some(ForthDb::new()),
            lazy_kernel: OnceLock::new(),
            vm_query: OnceLock::new(),
            projection_base: None,
            history: None,
            history_prefix: None,
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
        self.active_slot_count
    }

    pub fn record_count(&self) -> usize {
        self.record_count
    }

    pub fn resolve(&self, slot: &SlotId) -> Option<&Fact> {
        if self.eager_kernel.is_none() {
            self.vm_query().resolve(slot)
        } else {
            self.kernel().resolve(slot)
        }
    }

    pub fn definitions(&self, slot: &SlotId) -> Vec<&Fact> {
        if self.eager_kernel.is_none() {
            self.vm_query().definitions(slot)
        } else {
            self.kernel().definitions(slot)
        }
    }

    pub fn query(&self, patterns: &[Pattern], options: QueryOptions) -> QueryResult {
        if self.eager_kernel.is_none() {
            self.vm_query().query(patterns, options)
        } else {
            self.kernel().query(patterns, options)
        }
    }

    pub fn display_name(&self, entity: EntityId) -> String {
        if self.eager_kernel.is_none() {
            self.resolve(&ForthDb::display_slot(entity))
                .and_then(|fact| match &fact.object {
                    Atom::Literal(value) => Some(value.as_str().to_owned()),
                    Atom::Entity(_) => None,
                })
                .unwrap_or_else(|| entity.to_string())
        } else {
            self.kernel().display_name(entity)
        }
    }

    /// Whether the native query projection has already been built. VM-backed
    /// publication and recovery do not require it.
    pub fn is_query_projection_materialized(&self) -> bool {
        self.eager_kernel.is_some() || self.vm_query.get().is_some()
    }

    /// Materialize the native immutable-root query view on demand. VM-backed
    /// worlds do not construct the legacy `ForthDb` projection here.
    pub fn materialize_query_projection(&self) {
        if self.eager_kernel.is_none() {
            let _ = self.vm_query();
        } else {
            let _ = self.kernel();
        }
    }

    /// Whether this world owns an eager or lazily reconstructed legacy kernel.
    pub fn is_legacy_query_projection_materialized(&self) -> bool {
        self.eager_kernel.is_some() || self.lazy_kernel.get().is_some()
    }

    fn vm_query(&self) -> &VmQueryProjection {
        self.vm_query
            .get_or_init(|| {
                assert!(self.history.is_some(), "mapped VM roots preinstall their query view");
                Arc::new(VmQueryProjection::from_frames(&self.frames()))
            })
            .as_ref()
    }

    pub(crate) fn kernel(&self) -> &ForthDb {
        if let Some(kernel) = self.eager_kernel.as_ref() {
            return kernel;
        }
        self.lazy_kernel
            .get_or_init(|| {
                if self.history_prefix.is_some() {
                    let mut kernel = ForthDb::new();
                    let mut next_entity = 1;
                    for frame in self.frames() {
                        for operation in frame.operations.iter() {
                            apply_operation(&mut kernel, &mut next_entity, operation)
                                .expect("mapped history must materialize deterministically");
                        }
                    }
                    Arc::new(kernel)
                } else {
                    let history = self
                        .history
                        .as_deref()
                        .expect("only an eager genesis world may omit history");
                    history.materialize_kernel(self.projection_base.as_ref())
                }
            })
            .as_ref()
    }

    pub fn frames(&self) -> Vec<Arc<CommitFrame>> {
        let mut frames = self
            .history_prefix
            .as_ref()
            .map_or_else(Vec::new, |source| source.frames());
        let prefix_len = frames.len();
        let mut node = self.history.clone();
        while let Some(current) = node {
            frames.push(current.frame.clone());
            node = current.parent.clone();
        }
        frames[prefix_len..].reverse();
        frames
    }

    pub(crate) fn from_mmap(snapshot: Arc<MmapVmSnapshot>) -> Arc<Self> {
        let mut vm_query = OnceLock::new();
        let _ = vm_query.set(Arc::new(VmQueryProjection::from_mmap(snapshot.clone())));
        Arc::new(Self {
            id: snapshot.world_id(),
            version: snapshot.world_version(),
            next_entity: snapshot.next_entity(),
            operation_count: snapshot.operation_count(),
            active_slot_count: snapshot.active_slot_count(),
            record_count: snapshot.record_count(),
            eager_kernel: None,
            lazy_kernel: OnceLock::new(),
            vm_query,
            projection_base: None,
            history: None,
            history_prefix: Some(snapshot),
        })
    }

    fn reconstruct(frames: &[Arc<CommitFrame>]) -> Result<Self, CandidateError> {
        let mut world = Self::genesis();
        let mut kernel = ForthDb::new();
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
                apply_operation(&mut kernel, &mut next_entity, operation)?;
            }
            if next_entity != frame.resulting_allocator {
                return Err(CandidateError::AllocatorStateMismatch {
                    expected: frame.resulting_allocator,
                    actual: next_entity,
                });
            }
            kernel.validate().map_err(CandidateError::KernelInvariant)?;

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
            world.active_slot_count = kernel.active_slot_count();
            world.record_count = kernel.record_count();
            world.eager_kernel = None;
            world.lazy_kernel = OnceLock::new();
            world.vm_query = OnceLock::new();
            let history = Arc::new(HistoryNode {
                parent: world.history.clone(),
                frame: frame.clone(),
            });
            world.history = Some(history);
            world.history_prefix = None;
        }
        world.eager_kernel = Some(kernel);
        Ok(world)
    }

    pub(crate) fn from_vm_epoch(
        base: Arc<World>,
        operations: Vec<Operation>,
        next_entity: u64,
        active_slot_count: usize,
        record_count: usize,
    ) -> (Arc<Self>, Arc<CommitFrame>) {
        let projection_base = base
            .lazy_kernel
            .get()
            .cloned()
            .or_else(|| base.eager_kernel.as_ref().map(|kernel| Arc::new(kernel.clone())))
            .map(|kernel| ProjectionBase {
                kernel,
                version: base.version,
                next_entity: base.next_entity,
            })
            .or_else(|| base.projection_base.clone());
        let version = base.version + 1;
        let id = calculate_world_id(base.id, version, next_entity, &operations);
        let operations: Arc<[Operation]> = Arc::from(operations);
        let frame = Arc::new(CommitFrame {
            parent_world: base.id,
            resulting_world: id,
            parent_version: base.version,
            resulting_version: version,
            resulting_allocator: next_entity,
            operations: operations.clone(),
        });
        let world = Arc::new(Self {
            id,
            version,
            next_entity,
            operation_count: base.operation_count + operations.len(),
            active_slot_count,
            record_count,
            eager_kernel: None,
            lazy_kernel: OnceLock::new(),
            vm_query: OnceLock::new(),
            projection_base,
            history: Some(Arc::new(HistoryNode {
                parent: base.history.clone(),
                frame: frame.clone(),
            })),
            history_prefix: base.history_prefix.clone(),
        });
        (world, frame)
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
        Self::construct_from_state(
            base.id,
            base.version,
            base.next_entity,
            base.operation_count,
            base.kernel(),
            operations,
        )
    }

    pub(crate) fn construct_from_state(
        base_world: WorldId,
        base_version: u64,
        base_next_entity: u64,
        base_operation_count: usize,
        base_kernel: &ForthDb,
        operations: Vec<Operation>,
    ) -> Result<Self, CandidateError> {
        let mut kernel = base_kernel.clone();
        let mut next_entity = base_next_entity;
        for operation in &operations {
            apply_operation(&mut kernel, &mut next_entity, operation)?;
        }
        kernel.validate().map_err(CandidateError::KernelInvariant)?;

        let version = base_version + 1;
        let id = calculate_world_id(base_world, version, next_entity, &operations);
        Ok(Self {
            base_world,
            base_version,
            id,
            version,
            next_entity,
            base_operation_count,
            operations: Arc::from(operations),
            kernel,
        })
    }

    pub(crate) fn into_materialized_state(
        self,
    ) -> (WorldId, u64, u64, usize, Arc<[Operation]>, ForthDb) {
        (
            self.id,
            self.version,
            self.next_entity,
            self.base_operation_count + self.operations.len(),
            self.operations,
            self.kernel,
        )
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
        let active_slot_count = self.kernel.active_slot_count();
        let record_count = self.kernel.record_count();
        World {
            id: self.id,
            version: self.version,
            next_entity: self.next_entity,
            operation_count: self.base_operation_count + self.operations.len(),
            active_slot_count,
            record_count,
            eager_kernel: Some(self.kernel),
            lazy_kernel: OnceLock::new(),
            vm_query: OnceLock::new(),
            projection_base: None,
            history: Some(Arc::new(HistoryNode {
                parent: parent_history,
                frame,
            })),
            history_prefix: None,
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
                Atom::Literal(Literal::new(format!("{namespace}:{}", symbol.as_str()))),
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
            Self::Validation(message) => {
                write!(formatter, "candidate validation failed: {message}")
            }
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
    use forthdb_core::Variable;

    fn state_fact(entity: EntityId, value: &str) -> Fact {
        Fact::new(
            Atom::Entity(entity),
            Predicate::new("state"),
            Atom::Literal(Literal::new(value)),
        )
    }

    fn variable(name: &str) -> Variable {
        Variable::new(name).expect("valid test variable")
    }

    #[test]
    fn vm_root_queries_match_the_legacy_kernel_without_materializing_it() {
        let base = Arc::new(World::genesis());
        let first = EntityId::new(1);
        let second = EntityId::new(2);
        let first_kind = Fact::new(
            Atom::Entity(first),
            Predicate::new("kind"),
            Atom::Literal(Literal::new("book")),
        );
        let operations = vec![
            Operation::AllocateEntity { entity: first },
            Operation::AllocateEntity { entity: second },
            Operation::Define {
                slot: SlotId::new("book/1/kind"),
                fact: first_kind.clone(),
            },
            Operation::Define {
                slot: SlotId::new("book/1/kind/alias"),
                fact: first_kind.clone(),
            },
            Operation::Define {
                slot: SlotId::new("book/2/kind"),
                fact: Fact::new(
                    Atom::Entity(second),
                    Predicate::new("kind"),
                    Atom::Literal(Literal::new("book")),
                ),
            },
            Operation::Define {
                slot: SlotId::new("book/1/location"),
                fact: Fact::new(
                    Atom::Entity(first),
                    Predicate::new("location"),
                    Atom::Literal(Literal::new("shelf")),
                ),
            },
            Operation::Define {
                slot: SlotId::new("book/2/location"),
                fact: Fact::new(
                    Atom::Entity(second),
                    Predicate::new("location"),
                    Atom::Literal(Literal::new("desk")),
                ),
            },
            Operation::Define {
                slot: SlotId::new("book/2/location"),
                fact: Fact::new(
                    Atom::Entity(second),
                    Predicate::new("location"),
                    Atom::Literal(Literal::new("archive")),
                ),
            },
            Operation::Forget {
                slot: SlotId::new("book/2/location"),
            },
        ];

        let candidate = CandidateWorld::construct(&base, operations.clone()).expect("eager world");
        let active_slot_count = candidate.active_slot_count();
        let record_count = candidate.record_count();
        let frame = candidate.commit_frame();
        let eager = Arc::new(candidate.into_world(frame, base.history.clone()));
        let (vm, _) = World::from_vm_epoch(
            base,
            operations,
            3,
            active_slot_count,
            record_count,
        );

        let s = Term::Variable(variable("s"));
        let p = PredicateTerm::Variable(variable("p"));
        let o = Term::Variable(variable("o"));
        let entity = Term::Atom(Atom::Entity(first));
        let kind = PredicateTerm::Predicate(Predicate::new("kind"));
        let book = Term::Atom(Atom::Literal(Literal::new("book")));
        let patterns = vec![
            Pattern::new(entity.clone(), p.clone(), o.clone()),
            Pattern::new(s.clone(), kind.clone(), o.clone()),
            Pattern::new(s.clone(), p.clone(), book.clone()),
            Pattern::new(entity.clone(), kind.clone(), o.clone()),
            Pattern::new(entity.clone(), p.clone(), book.clone()),
            Pattern::new(s.clone(), kind.clone(), book.clone()),
            Pattern::new(entity.clone(), kind.clone(), book.clone()),
        ];
        let options = QueryOptions {
            optimize: true,
            distinct: false,
            include_provenance: true,
            limit: None,
        };
        for pattern in patterns {
            assert_eq!(vm.query(&[pattern.clone()], options), eager.query(&[pattern], options));
        }

        let join = vec![
            Pattern::new(s.clone(), kind, book),
            Pattern::new(
                s.clone(),
                PredicateTerm::Predicate(Predicate::new("location")),
                Term::Variable(variable("place")),
            ),
            Pattern::new(s.clone(), p, o),
        ];
        for optimize in [false, true] {
            for distinct in [false, true] {
                let options = QueryOptions {
                    optimize,
                    distinct,
                    include_provenance: true,
                    limit: Some(3),
                };
                assert_eq!(vm.query(&join, options), eager.query(&join, options));
            }
        }
        assert_eq!(
            vm.definitions(&SlotId::new("book/2/location")),
            eager.definitions(&SlotId::new("book/2/location"))
        );
        assert!(vm.is_query_projection_materialized());
        assert!(!vm.is_legacy_query_projection_materialized());
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
        let error = database.commit(stale).expect_err("stale writer must abort");
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
        let candidate = update
            .candidate()
            .expect("candidate should read staged writes");
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
            let database = Database::new(MemoryCommitStore::new()).expect("empty store is valid");
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
