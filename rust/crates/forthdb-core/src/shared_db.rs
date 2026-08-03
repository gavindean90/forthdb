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
