type Index<K> = ShardedMap<K, RecordSet, INDEX_SHARDS>;

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
    let mut bucket = index.get(&key).cloned().unwrap_or_default();
    bucket.insert(record_id);
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
    bucket.remove(&record_id);
    if bucket.is_empty() {
        index.remove(key);
    } else {
        index.insert(key.clone(), bucket);
    }
}
