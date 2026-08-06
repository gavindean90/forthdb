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
    active_signature: ActiveSignature,
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
        self.active_signature.add(record_id);
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
        self.active_signature.remove(record_id);
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

    fn validate_fast(&self, store: &SharedDefinitionStore) -> Result<(), String> {
        if self.active_signature != store.active_signature() {
            return Err("current indexes do not match active slot signature".to_owned());
        }
        Ok(())
    }

    fn validate_full(&self, store: &SharedDefinitionStore) -> Result<(), String> {
        let active = store.active_record_ids();
        let indexed: BTreeSet<_> = self
            .by_subject_predicate
            .values()
            .flat_map(|bucket| bucket.iter().copied())
            .collect();
        if active != indexed {
            return Err("current indexes do not match active slot heads".to_owned());
        }
        let mut signature = ActiveSignature::default();
        for record_id in indexed {
            signature.add(record_id);
        }
        if signature != self.active_signature {
            return Err("current-index signature does not match full audit".to_owned());
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

    fn replace(
        &mut self,
        old_record_id: RecordId,
        new_record_id: RecordId,
        store: &SharedDefinitionStore,
    ) {
        let old_fact = store
            .record(old_record_id)
            .fact
            .as_ref()
            .expect("current definitions have facts");
        let new_fact = store
            .record(new_record_id)
            .fact
            .as_ref()
            .expect("new definitions have facts");

        if old_fact.subject == new_fact.subject {
            replace_index(&mut self.by_subject, &old_fact.subject, old_record_id, new_record_id);
        } else {
            remove_index(&mut self.by_subject, &old_fact.subject, old_record_id);
            insert_index(&mut self.by_subject, new_fact.subject.clone(), new_record_id);
        }

        if old_fact.predicate == new_fact.predicate {
            replace_index(&mut self.by_predicate, &old_fact.predicate, old_record_id, new_record_id);
        } else {
            remove_index(&mut self.by_predicate, &old_fact.predicate, old_record_id);
            insert_index(&mut self.by_predicate, new_fact.predicate.clone(), new_record_id);
        }

        if old_fact.object == new_fact.object {
            replace_index(&mut self.by_object, &old_fact.object, old_record_id, new_record_id);
        } else {
            remove_index(&mut self.by_object, &old_fact.object, old_record_id);
            insert_index(&mut self.by_object, new_fact.object.clone(), new_record_id);
        }

        let old_sp = (old_fact.subject.clone(), old_fact.predicate.clone());
        let new_sp = (new_fact.subject.clone(), new_fact.predicate.clone());
        if old_sp == new_sp {
            replace_index(&mut self.by_subject_predicate, &old_sp, old_record_id, new_record_id);
        } else {
            remove_index(&mut self.by_subject_predicate, &old_sp, old_record_id);
            insert_index(&mut self.by_subject_predicate, new_sp, new_record_id);
        }

        let old_so = (old_fact.subject.clone(), old_fact.object.clone());
        let new_so = (new_fact.subject.clone(), new_fact.object.clone());
        if old_so == new_so {
            replace_index(&mut self.by_subject_object, &old_so, old_record_id, new_record_id);
        } else {
            remove_index(&mut self.by_subject_object, &old_so, old_record_id);
            insert_index(&mut self.by_subject_object, new_so, new_record_id);
        }

        let old_po = (old_fact.predicate.clone(), old_fact.object.clone());
        let new_po = (new_fact.predicate.clone(), new_fact.object.clone());
        if old_po == new_po {
            replace_index(&mut self.by_predicate_object, &old_po, old_record_id, new_record_id);
        } else {
            remove_index(&mut self.by_predicate_object, &old_po, old_record_id);
            insert_index(&mut self.by_predicate_object, new_po, new_record_id);
        }

        if old_fact == new_fact {
            replace_index(&mut self.by_exact, old_fact, old_record_id, new_record_id);
        } else {
            remove_index(&mut self.by_exact, old_fact, old_record_id);
            insert_index(&mut self.by_exact, new_fact.clone(), new_record_id);
        }

        self.active_signature.remove(old_record_id);
        self.active_signature.add(new_record_id);
    }
}

fn insert_index<K>(index: &mut Index<K>, key: K, record_id: RecordId)
where
    K: Clone + Eq + Hash,
{
    if let Some(bucket) = index.get_mut(&key) {
        bucket.insert(record_id);
        return;
    }

    let mut bucket = RecordSet::default();
    bucket.insert(record_id);
    index.insert(key, bucket);
}

fn remove_index<K>(index: &mut Index<K>, key: &K, record_id: RecordId)
where
    K: Clone + Eq + Hash,
{
    if let Some(bucket) = index.get_mut(key) {
        bucket.remove(&record_id);
        if bucket.is_empty() {
            index.remove(key);
        }
    }
}

fn replace_index<K>(
    index: &mut Index<K>,
    key: &K,
    old_record_id: RecordId,
    new_record_id: RecordId,
) where
    K: Clone + Eq + Hash,
{
    if let Some(bucket) = index.get_mut(key) {
        bucket.remove(&old_record_id);
        bucket.insert(new_record_id);
    }
}
