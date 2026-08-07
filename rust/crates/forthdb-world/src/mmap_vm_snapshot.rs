use super::*;
use crate::stack_vm::{Cell, FactRecord, NONE, RecordKind, SlotToken};
use memmap2::{Mmap, MmapOptions};
use std::cmp::Ordering as CompareOrdering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

const MAGIC: &[u8; 8] = b"FTHVMS01";
const TRAILER: &[u8; 8] = b"VMSEND01";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 320;
const SECTION_COUNT: usize = 10;
const SECTION_TABLE_OFFSET: usize = 112;
const RECORD_LEN: usize = 48;
const INDEX_ENTRY_LEN: usize = 32;
const MAX_SNAPSHOT_BYTES: usize = 512 * 1024 * 1024;
const VM_LITERAL_BASE: u64 = 1 << 63;
const CHECKSUM_OFFSET: u64 = 0xcbf29ce484222325;
const CHECKSUM_PRIME: u64 = 0x100000001b3;

const SLOTS: usize = 0;
const PREDICATES: usize = 1;
const LITERALS: usize = 2;
const RECORDS: usize = 3;
const HEADS: usize = 4;
const ACTIVE: usize = 5;
const SPO: usize = 6;
const POS: usize = 7;
const OSP: usize = 8;
const FRAMES: usize = 9;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmapSnapshotMetadata {
    pub epoch_count: u64,
    pub journal_offset: u64,
    pub snapshot_bytes: u64,
    pub world_id: WorldId,
    pub world_version: u64,
}

#[derive(Clone, Copy)]
struct MappedRecord {
    original_id: u64,
    slot: u32,
    previous: u32,
    subject: u64,
    predicate: u32,
    object: u64,
}

#[derive(Clone, Copy)]
struct IndexEntry {
    first: u64,
    second: u64,
    third: u64,
    record: u32,
}

#[derive(Clone)]
struct StringTable {
    section: Range<usize>,
    offsets: Arc<[usize]>,
    blob_start: usize,
}

impl StringTable {
    fn parse(mapping: &[u8], section: Range<usize>) -> Result<Self, String> {
        let bytes = mapping
            .get(section.clone())
            .ok_or_else(|| "string-table section is out of range".to_owned())?;
        if bytes.len() < 8 {
            return Err("truncated string-table header".to_owned());
        }
        let count = read_u32_at(bytes, 0)? as usize;
        let offset_bytes = (count + 1)
            .checked_mul(8)
            .ok_or_else(|| "string-table offset overflow".to_owned())?;
        let blob_start = 8usize
            .checked_add(offset_bytes)
            .ok_or_else(|| "string-table header overflow".to_owned())?;
        if blob_start > bytes.len() {
            return Err("truncated string-table offsets".to_owned());
        }
        let mut offsets = Vec::with_capacity(count + 1);
        for index in 0..=count {
            let value = read_u64_at(bytes, 8 + index * 8)?;
            offsets.push(
                usize::try_from(value)
                    .map_err(|_| "string-table offset exceeds address space".to_owned())?,
            );
        }
        if offsets.first().copied() != Some(0)
            || offsets.windows(2).any(|pair| pair[0] > pair[1])
            || offsets.last().copied().unwrap_or(0) != bytes.len() - blob_start
        {
            return Err("invalid string-table offset directory".to_owned());
        }
        let table = Self {
            section,
            offsets: Arc::from(offsets),
            blob_start,
        };
        let mut previous = None;
        for index in 0..table.len() {
            let value = table.value(mapping, index)?;
            if previous.is_some_and(|prior: &str| prior >= value) {
                return Err("string-table values are not strictly sorted".to_owned());
            }
            previous = Some(value);
        }
        Ok(table)
    }

    fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    fn value<'a>(&self, mapping: &'a [u8], index: usize) -> Result<&'a str, String> {
        let start = *self
            .offsets
            .get(index)
            .ok_or_else(|| "string token is out of range".to_owned())?;
        let end = *self
            .offsets
            .get(index + 1)
            .ok_or_else(|| "string token terminator is out of range".to_owned())?;
        let absolute = self.section.start + self.blob_start;
        std::str::from_utf8(
            mapping
                .get(absolute + start..absolute + end)
                .ok_or_else(|| "string token bytes are out of range".to_owned())?,
        )
        .map_err(|error| error.to_string())
    }

    fn lookup(&self, mapping: &[u8], value: &str) -> Option<u32> {
        let mut low = 0usize;
        let mut high = self.len();
        while low < high {
            let middle = low + (high - low) / 2;
            let candidate = self.value(mapping, middle).ok()?;
            match candidate.cmp(value) {
                CompareOrdering::Less => low = middle + 1,
                CompareOrdering::Greater => high = middle,
                CompareOrdering::Equal => return u32::try_from(middle).ok(),
            }
        }
        None
    }
}

pub(crate) struct MmapVmSnapshot {
    mapping: Mmap,
    path: PathBuf,
    epoch_count: u64,
    journal_offset: u64,
    world_id: WorldId,
    world_version: u64,
    next_entity: u64,
    operation_count: usize,
    active_slot_count: usize,
    record_count: usize,
    frame_count: usize,
    sections: [Range<usize>; SECTION_COUNT],
    slots: StringTable,
    predicates: StringTable,
    literals: StringTable,
    facts: Vec<OnceLock<Fact>>,
    decoded_frames: OnceLock<Vec<Arc<CommitFrame>>>,
}

impl fmt::Debug for MmapVmSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MmapVmSnapshot")
            .field("path", &self.path)
            .field("bytes", &self.mapping.len())
            .field("world", &self.world_id)
            .field("version", &self.world_version)
            .field("records", &self.retained_record_count())
            .finish()
    }
}

impl MmapVmSnapshot {
    pub(crate) fn path_for(journal: &Path) -> PathBuf {
        let mut value = journal.as_os_str().to_os_string();
        value.push(".vm-snapshot");
        PathBuf::from(value)
    }

    fn temporary_path_for(journal: &Path) -> PathBuf {
        let mut value = journal.as_os_str().to_os_string();
        value.push(".vm-snapshot.tmp");
        PathBuf::from(value)
    }

    pub(crate) fn create(
        journal_path: &Path,
        journal: &[u8],
        epoch_count: u64,
        world: &World,
        projection: &VmQueryProjection,
    ) -> Result<MmapSnapshotMetadata, String> {
        if projection.mapped.is_some() {
            return Err("rewriting a mapped snapshot with a tail is not implemented".to_owned());
        }
        let mut slot_names = projection
            .definitions
            .keys()
            .map(|slot| slot.as_str().to_owned())
            .collect::<Vec<_>>();
        slot_names.sort();
        let mut predicates = BTreeSet::new();
        let mut literals = BTreeSet::new();
        for stack in projection.definitions.values() {
            for entry in stack {
                predicates.insert(entry.fact.predicate.as_str().to_owned());
                collect_atom_string(&entry.fact.subject, &mut literals);
                collect_atom_string(&entry.fact.object, &mut literals);
            }
        }
        let predicate_names = predicates.into_iter().collect::<Vec<_>>();
        let literal_names = literals.into_iter().collect::<Vec<_>>();
        let slot_tokens = slot_names
            .iter()
            .enumerate()
            .map(|(index, value)| (value.as_str(), index as u32))
            .collect::<BTreeMap<_, _>>();
        let predicate_tokens = predicate_names
            .iter()
            .enumerate()
            .map(|(index, value)| (value.as_str(), index as u32))
            .collect::<BTreeMap<_, _>>();
        let literal_tokens = literal_names
            .iter()
            .enumerate()
            .map(|(index, value)| (value.as_str(), index as u32))
            .collect::<BTreeMap<_, _>>();

        let mut records = Vec::<MappedRecord>::new();
        let mut heads = vec![NONE; slot_names.len()];
        for slot_name in &slot_names {
            let token = slot_tokens[slot_name.as_str()];
            let slot = SlotId::new(slot_name.clone());
            let mut previous = NONE;
            if let Some(stack) = projection.definitions.get(&slot) {
                for entry in stack {
                    let index = records.len() as u32;
                    records.push(MappedRecord {
                        original_id: entry.record_id as u64,
                        slot: token,
                        previous,
                        subject: atom_cell(&entry.fact.subject, &literal_tokens)?,
                        predicate: predicate_tokens[entry.fact.predicate.as_str()],
                        object: atom_cell(&entry.fact.object, &literal_tokens)?,
                    });
                    previous = index;
                }
            }
            heads[token as usize] = previous;
        }
        let mut active = heads
            .iter()
            .copied()
            .filter(|head| *head != NONE)
            .collect::<Vec<_>>();
        active.sort_unstable_by_key(|record| records[*record as usize].original_id);
        let mut spo = Vec::with_capacity(active.len());
        let mut pos = Vec::with_capacity(active.len());
        let mut osp = Vec::with_capacity(active.len());
        for record in active.iter().copied() {
            let fact = records[record as usize];
            spo.push(IndexEntry {
                first: fact.subject,
                second: u64::from(fact.predicate),
                third: fact.object,
                record,
            });
            pos.push(IndexEntry {
                first: u64::from(fact.predicate),
                second: fact.object,
                third: fact.subject,
                record,
            });
            osp.push(IndexEntry {
                first: fact.object,
                second: fact.subject,
                third: u64::from(fact.predicate),
                record,
            });
        }
        let order = |left: &IndexEntry, right: &IndexEntry| {
            (left.first, left.second, left.third)
                .cmp(&(right.first, right.second, right.third))
                .then_with(|| {
                    records[left.record as usize]
                        .original_id
                        .cmp(&records[right.record as usize].original_id)
                })
        };
        spo.sort_unstable_by(order);
        pos.sort_unstable_by(order);
        osp.sort_unstable_by(order);

        let frames = world.frames();
        let section_bytes = [
            encode_string_table(&slot_names),
            encode_string_table(&predicate_names),
            encode_string_table(&literal_names),
            encode_records(&records),
            encode_u32s(&heads),
            encode_u32s(&active),
            encode_indexes(&spo),
            encode_indexes(&pos),
            encode_indexes(&osp),
            encode_frames(&frames)?,
        ];
        let mut bytes = vec![0u8; HEADER_LEN];
        let mut ranges = Vec::with_capacity(SECTION_COUNT);
        for section in section_bytes {
            align_eight(&mut bytes);
            let start = bytes.len();
            bytes.extend_from_slice(&section);
            ranges.push(start..bytes.len());
        }
        bytes.extend_from_slice(TRAILER);
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err("mmap VM snapshot exceeds size limit".to_owned());
        }
        bytes[0..8].copy_from_slice(MAGIC);
        put_u32(&mut bytes, 8, VERSION);
        put_u32(&mut bytes, 12, HEADER_LEN as u32);
        let file_len = bytes.len() as u64;
        put_u64(&mut bytes, 16, file_len);
        let body_digest = digest(&bytes[HEADER_LEN..]);
        put_u64(&mut bytes, 24, body_digest);
        put_u64(&mut bytes, 32, epoch_count);
        put_u64(&mut bytes, 40, journal.len() as u64);
        put_u64(&mut bytes, 48, digest(journal));
        put_u64(&mut bytes, 56, world.id().value());
        put_u64(&mut bytes, 64, world.version());
        put_u64(&mut bytes, 72, world.next_entity());
        put_u64(&mut bytes, 80, world.operation_count() as u64);
        put_u64(&mut bytes, 88, world.active_slot_count() as u64);
        put_u64(&mut bytes, 96, world.record_count() as u64);
        put_u64(&mut bytes, 104, frames.len() as u64);
        for (index, range) in ranges.iter().enumerate() {
            put_u64(
                &mut bytes,
                SECTION_TABLE_OFFSET + index * 16,
                range.start as u64,
            );
            put_u64(
                &mut bytes,
                SECTION_TABLE_OFFSET + index * 16 + 8,
                range.len() as u64,
            );
        }

        let temporary = Self::temporary_path_for(journal_path);
        let target = Self::path_for(journal_path);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.sync_data().map_err(|error| error.to_string())?;
        drop(file);
        fs::rename(&temporary, &target).map_err(|error| error.to_string())?;
        if let Some(parent) = target.parent() {
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| error.to_string())?;
        }
        Ok(MmapSnapshotMetadata {
            epoch_count,
            journal_offset: journal.len() as u64,
            snapshot_bytes: file_len,
            world_id: world.id(),
            world_version: world.version(),
        })
    }

    pub(crate) fn open(journal_path: &Path, journal: &[u8]) -> Result<Arc<Self>, String> {
        let path = Self::path_for(journal_path);
        let file = File::open(&path).map_err(|error| error.to_string())?;
        let mapping =
            unsafe { MmapOptions::new().map(&file) }.map_err(|error| error.to_string())?;
        if mapping.len() < HEADER_LEN + TRAILER.len()
            || mapping.len() > MAX_SNAPSHOT_BYTES
            || &mapping[0..8] != MAGIC
            || read_u32_at(&mapping, 8)? != VERSION
            || read_u32_at(&mapping, 12)? as usize != HEADER_LEN
            || read_u64_at(&mapping, 16)? as usize != mapping.len()
            || &mapping[mapping.len() - TRAILER.len()..] != TRAILER
            || read_u64_at(&mapping, 24)? != digest(&mapping[HEADER_LEN..])
        {
            return Err("invalid mmap VM snapshot envelope".to_owned());
        }
        let epoch_count = read_u64_at(&mapping, 32)?;
        let journal_offset = read_u64_at(&mapping, 40)?;
        let journal_digest = read_u64_at(&mapping, 48)?;
        let offset = usize::try_from(journal_offset)
            .map_err(|_| "snapshot journal offset exceeds address space".to_owned())?;
        if offset < 16 || offset > journal.len() || digest(&journal[..offset]) != journal_digest {
            return Err("mmap VM snapshot does not match the journal prefix".to_owned());
        }
        let mut sections_vec = Vec::with_capacity(SECTION_COUNT);
        for index in 0..SECTION_COUNT {
            let start = read_u64_at(&mapping, SECTION_TABLE_OFFSET + index * 16)?;
            let len = read_u64_at(&mapping, SECTION_TABLE_OFFSET + index * 16 + 8)?;
            let start = usize::try_from(start)
                .map_err(|_| "snapshot section offset exceeds address space".to_owned())?;
            let len = usize::try_from(len)
                .map_err(|_| "snapshot section length exceeds address space".to_owned())?;
            let end = start
                .checked_add(len)
                .ok_or_else(|| "snapshot section length overflow".to_owned())?;
            if start < HEADER_LEN || end > mapping.len() - TRAILER.len() {
                return Err("snapshot section is out of range".to_owned());
            }
            sections_vec.push(start..end);
        }
        let mut ordered = sections_vec.clone();
        ordered.sort_by_key(|range| range.start);
        if ordered.windows(2).any(|pair| pair[0].end > pair[1].start) {
            return Err("snapshot sections overlap".to_owned());
        }
        let sections: [Range<usize>; SECTION_COUNT] = sections_vec
            .try_into()
            .map_err(|_| "snapshot section directory is incomplete".to_owned())?;
        let slots = StringTable::parse(&mapping, sections[SLOTS].clone())?;
        let predicates = StringTable::parse(&mapping, sections[PREDICATES].clone())?;
        let literals = StringTable::parse(&mapping, sections[LITERALS].clone())?;
        if sections[RECORDS].len() % RECORD_LEN != 0
            || sections[HEADS].len() != slots.len() * 4
            || sections[ACTIVE].len() % 4 != 0
            || sections[SPO].len() % INDEX_ENTRY_LEN != 0
            || sections[POS].len() % INDEX_ENTRY_LEN != 0
            || sections[OSP].len() % INDEX_ENTRY_LEN != 0
        {
            return Err("snapshot fixed-width section has an invalid length".to_owned());
        }
        let retained = sections[RECORDS].len() / RECORD_LEN;
        let active_count = sections[ACTIVE].len() / 4;
        if active_count != read_u64_at(&mapping, 88)? as usize
            || sections[SPO].len() / INDEX_ENTRY_LEN != active_count
            || sections[POS].len() / INDEX_ENTRY_LEN != active_count
            || sections[OSP].len() / INDEX_ENTRY_LEN != active_count
        {
            return Err("snapshot active index counts disagree".to_owned());
        }
        let world_id = WorldId::new(read_u64_at(&mapping, 56)?);
        let world_version = read_u64_at(&mapping, 64)?;
        let next_entity = read_u64_at(&mapping, 72)?;
        let operation_count = usize::try_from(read_u64_at(&mapping, 80)?)
            .map_err(|_| "snapshot operation count exceeds address space".to_owned())?;
        let record_count = usize::try_from(read_u64_at(&mapping, 96)?)
            .map_err(|_| "snapshot record count exceeds address space".to_owned())?;
        let frame_count = usize::try_from(read_u64_at(&mapping, 104)?)
            .map_err(|_| "snapshot frame count exceeds address space".to_owned())?;
        if sections[FRAMES].len() < 8
            || usize::try_from(read_u64_at(&mapping[sections[FRAMES].clone()], 0)?)
                .map_err(|_| "snapshot history count exceeds address space".to_owned())?
                != frame_count
        {
            return Err("snapshot history count disagrees with metadata".to_owned());
        }
        let snapshot = Arc::new(Self {
            mapping,
            path,
            epoch_count,
            journal_offset,
            world_id,
            world_version,
            next_entity,
            operation_count,
            active_slot_count: active_count,
            record_count,
            frame_count,
            sections,
            slots,
            predicates,
            literals,
            facts: (0..retained).map(|_| OnceLock::new()).collect(),
            decoded_frames: OnceLock::new(),
        });
        snapshot.validate_records_and_indexes()?;
        Ok(snapshot)
    }

    fn validate_records_and_indexes(&self) -> Result<(), String> {
        for index in 0..self.retained_record_count() {
            let record = self.record(index as u32)?;
            if record.slot as usize >= self.slot_count()
                || record.predicate as usize >= self.predicate_count()
                || (record.previous != NONE && record.previous as usize >= index)
                || (record.subject >= VM_LITERAL_BASE
                    && (record.subject - VM_LITERAL_BASE) as usize >= self.literal_count())
                || (record.object >= VM_LITERAL_BASE
                    && (record.object - VM_LITERAL_BASE) as usize >= self.literal_count())
            {
                return Err(format!("invalid mapped fact record {index}"));
            }
        }
        for token in 0..self.slot_count() {
            let head = self.head(token as u32)?;
            if head != NONE && head as usize >= self.retained_record_count() {
                return Err("mapped slot head exceeds record section".to_owned());
            }
        }
        for section in [SPO, POS, OSP] {
            let mut previous = None;
            for index in 0..self.index_count(section) {
                let entry = self.index_entry(section, index)?;
                if entry.record as usize >= self.retained_record_count() {
                    return Err("mapped index record exceeds arena".to_owned());
                }
                let key = (entry.first, entry.second, entry.third);
                if previous.is_some_and(|value| value > key) {
                    return Err("mapped index is not sorted".to_owned());
                }
                previous = Some(key);
            }
        }
        Ok(())
    }

    pub(crate) fn epoch_count(&self) -> u64 {
        self.epoch_count
    }

    pub(crate) fn journal_offset(&self) -> u64 {
        self.journal_offset
    }

    pub(crate) fn world_id(&self) -> WorldId {
        self.world_id
    }

    pub(crate) fn world_version(&self) -> u64 {
        self.world_version
    }

    pub(crate) fn next_entity(&self) -> u64 {
        self.next_entity
    }

    pub(crate) fn operation_count(&self) -> usize {
        self.operation_count
    }

    pub(crate) fn active_slot_count(&self) -> usize {
        self.active_slot_count
    }

    pub(crate) fn record_count(&self) -> usize {
        self.record_count
    }

    pub(crate) fn snapshot_bytes(&self) -> u64 {
        self.mapping.len() as u64
    }

    pub(crate) fn slot_count(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn predicate_count(&self) -> usize {
        self.predicates.len()
    }

    pub(crate) fn literal_count(&self) -> usize {
        self.literals.len()
    }

    pub(crate) fn slot_token(&self, value: &str) -> Option<u32> {
        self.slots.lookup(&self.mapping, value)
    }

    pub(crate) fn predicate_token(&self, value: &str) -> Option<u32> {
        self.predicates.lookup(&self.mapping, value)
    }

    pub(crate) fn literal_token(&self, value: &str) -> Option<u32> {
        self.literals.lookup(&self.mapping, value)
    }

    pub(crate) fn predicate_value(&self, token: usize) -> Option<Predicate> {
        self.predicates
            .value(&self.mapping, token)
            .ok()
            .map(Predicate::new)
    }

    pub(crate) fn literal_value(&self, token: usize) -> Option<Literal> {
        self.literals
            .value(&self.mapping, token)
            .ok()
            .map(Literal::new)
    }

    pub(crate) fn workspace_image(&self) -> Result<(Vec<FactRecord>, Vec<u32>), String> {
        let mut records = Vec::with_capacity(self.retained_record_count());
        for index in 0..self.retained_record_count() {
            let record = self.record(index as u32)?;
            records.push(FactRecord {
                kind: RecordKind::Define,
                slot: SlotToken(record.slot),
                subject: Cell(record.subject),
                predicate: Cell(u64::from(record.predicate)),
                object: Cell(record.object),
                previous_visible: record.previous,
                resulting_visible: index as u32,
            });
        }
        let mut heads = Vec::with_capacity(self.slot_count());
        for token in 0..self.slot_count() {
            heads.push(self.head(token as u32)?);
        }
        Ok((records, heads))
    }

    pub(crate) fn resolve(&self, slot: &SlotId) -> Option<&Fact> {
        let token = self.slot_token(slot.as_str())?;
        let head = self.head(token).ok()?;
        (head != NONE).then(|| self.fact(head).ok()).flatten()
    }

    pub(crate) fn definitions(&self, slot: &SlotId) -> Vec<&Fact> {
        let Some(token) = self.slot_token(slot.as_str()) else {
            return Vec::new();
        };
        let mut output = Vec::new();
        let mut record = self.head(token).unwrap_or(NONE);
        while record != NONE {
            let Ok(fact) = self.fact(record) else {
                break;
            };
            output.push(fact);
            record = self.record(record).map_or(NONE, |entry| entry.previous);
        }
        output
    }

    pub(crate) fn display_name(&self, entity: EntityId) -> String {
        self.resolve(&ForthDb::display_slot(entity))
            .and_then(|fact| match &fact.object {
                Atom::Literal(value) => Some(value.as_str().to_owned()),
                Atom::Entity(_) => None,
            })
            .unwrap_or_else(|| entity.to_string())
    }

    fn retained_record_count(&self) -> usize {
        self.sections[RECORDS].len() / RECORD_LEN
    }

    fn record(&self, index: u32) -> Result<MappedRecord, String> {
        let start = self.sections[RECORDS].start + index as usize * RECORD_LEN;
        let bytes = self
            .mapping
            .get(start..start + RECORD_LEN)
            .ok_or_else(|| "mapped fact record is out of range".to_owned())?;
        Ok(MappedRecord {
            original_id: read_u64_at(bytes, 0)?,
            slot: read_u32_at(bytes, 8)?,
            previous: read_u32_at(bytes, 12)?,
            subject: read_u64_at(bytes, 16)?,
            predicate: read_u32_at(bytes, 24)?,
            object: read_u64_at(bytes, 32)?,
        })
    }

    fn fact(&self, index: u32) -> Result<&Fact, String> {
        let cache = self
            .facts
            .get(index as usize)
            .ok_or_else(|| "mapped fact cache index is out of range".to_owned())?;
        if let Some(fact) = cache.get() {
            return Ok(fact);
        }
        let record = self.record(index)?;
        let fact = Fact::new(
            self.atom_value(record.subject)?,
            self.predicate_value(record.predicate as usize)
                .ok_or_else(|| "mapped predicate token is invalid".to_owned())?,
            self.atom_value(record.object)?,
        );
        let _ = cache.set(fact);
        cache
            .get()
            .ok_or_else(|| "mapped fact cache initialization failed".to_owned())
    }

    fn atom_value(&self, cell: u64) -> Result<Atom, String> {
        if cell < VM_LITERAL_BASE {
            Ok(Atom::Entity(EntityId::new(cell)))
        } else {
            self.literal_value((cell - VM_LITERAL_BASE) as usize)
                .map(Atom::Literal)
                .ok_or_else(|| "mapped literal token is invalid".to_owned())
        }
    }

    fn head(&self, token: u32) -> Result<u32, String> {
        read_u32_at(
            &self.mapping[self.sections[HEADS].clone()],
            token as usize * 4,
        )
    }

    fn active_record(&self, index: usize) -> Result<u32, String> {
        read_u32_at(&self.mapping[self.sections[ACTIVE].clone()], index * 4)
    }

    fn index_count(&self, section: usize) -> usize {
        self.sections[section].len() / INDEX_ENTRY_LEN
    }

    fn index_entry(&self, section: usize, index: usize) -> Result<IndexEntry, String> {
        let start = self.sections[section].start + index * INDEX_ENTRY_LEN;
        let bytes = self
            .mapping
            .get(start..start + INDEX_ENTRY_LEN)
            .ok_or_else(|| "mapped index entry is out of range".to_owned())?;
        Ok(IndexEntry {
            first: read_u64_at(bytes, 0)?,
            second: read_u64_at(bytes, 8)?,
            third: read_u64_at(bytes, 16)?,
            record: read_u32_at(bytes, 24)?,
        })
    }

    pub(crate) fn query(&self, patterns: &[Pattern], options: QueryOptions) -> QueryResult {
        let mut output = Vec::<MappedQueryFrame>::new();
        let mut first_path = Vec::<Pattern>::new();
        let mut metrics = QueryMetrics::default();
        self.walk_query(
            MappedQueryFrame {
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
        frame: MappedQueryFrame,
        remaining: Vec<Pattern>,
        depth: usize,
        options: QueryOptions,
        output: &mut Vec<MappedQueryFrame>,
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
                .min_by_key(|(_, pattern)| self.candidate_count(pattern, &frame.binding))
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
        for record in candidates {
            let Ok(record_value) = self.record(record) else {
                continue;
            };
            if let Some(binding) = self.unify(&chosen, record_value, &frame.binding) {
                metrics.bindings_emitted += 1;
                let mut provenance = frame.provenance.clone();
                let slot = self
                    .slots
                    .value(&self.mapping, record_value.slot as usize)
                    .expect("validated slot token");
                provenance.push(SlotId::new(slot));
                if self.walk_query(
                    MappedQueryFrame {
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

    fn candidate_count(&self, pattern: &Pattern, binding: &Binding) -> usize {
        self.candidate_spec(pattern, binding)
            .map_or(0, |spec| match spec {
                CandidateSpec::All => self.active_slot_count,
                CandidateSpec::Range(section, key, length) => {
                    let (start, end) = self.index_range(section, key, length);
                    end - start
                }
                CandidateSpec::Empty => 0,
            })
    }

    fn candidates(&self, pattern: &Pattern, binding: &Binding) -> Vec<u32> {
        match self.candidate_spec(pattern, binding) {
            Some(CandidateSpec::All) => (0..self.active_slot_count)
                .filter_map(|index| self.active_record(index).ok())
                .collect(),
            Some(CandidateSpec::Range(section, key, length)) => {
                let (start, end) = self.index_range(section, key, length);
                (start..end)
                    .filter_map(|index| self.index_entry(section, index).ok())
                    .map(|entry| entry.record)
                    .collect()
            }
            Some(CandidateSpec::Empty) | None => Vec::new(),
        }
    }

    fn candidate_spec(&self, pattern: &Pattern, binding: &Binding) -> Option<CandidateSpec> {
        let subject = self.resolved_atom_cell(&pattern.subject, binding)?;
        let predicate = self.resolved_predicate_cell(&pattern.predicate, binding)?;
        let object = self.resolved_atom_cell(&pattern.object, binding)?;
        Some(match (subject, predicate, object) {
            (Resolved::Missing, _, _) | (_, Resolved::Missing, _) | (_, _, Resolved::Missing) => {
                CandidateSpec::Empty
            }
            (Resolved::Value(subject), Resolved::Value(predicate), Resolved::Value(object)) => {
                CandidateSpec::Range(SPO, [subject, predicate, object], 3)
            }
            (Resolved::Value(subject), Resolved::Value(predicate), Resolved::Unbound) => {
                CandidateSpec::Range(SPO, [subject, predicate, 0], 2)
            }
            (Resolved::Value(subject), Resolved::Unbound, Resolved::Value(object)) => {
                CandidateSpec::Range(OSP, [object, subject, 0], 2)
            }
            (Resolved::Unbound, Resolved::Value(predicate), Resolved::Value(object)) => {
                CandidateSpec::Range(POS, [predicate, object, 0], 2)
            }
            (Resolved::Value(subject), Resolved::Unbound, Resolved::Unbound) => {
                CandidateSpec::Range(SPO, [subject, 0, 0], 1)
            }
            (Resolved::Unbound, Resolved::Value(predicate), Resolved::Unbound) => {
                CandidateSpec::Range(POS, [predicate, 0, 0], 1)
            }
            (Resolved::Unbound, Resolved::Unbound, Resolved::Value(object)) => {
                CandidateSpec::Range(OSP, [object, 0, 0], 1)
            }
            (Resolved::Unbound, Resolved::Unbound, Resolved::Unbound) => CandidateSpec::All,
        })
    }

    fn resolved_atom_cell(&self, term: &Term, binding: &Binding) -> Option<Resolved> {
        match term {
            Term::Atom(atom) => Some(self.atom_cell_for_query(atom)),
            Term::Variable(variable) => match binding.get(variable.as_str()) {
                Some(value) => value
                    .as_atom()
                    .map(|atom| self.atom_cell_for_query(&atom))
                    .or(Some(Resolved::Missing)),
                None => Some(Resolved::Unbound),
            },
        }
    }

    fn resolved_predicate_cell(&self, term: &PredicateTerm, binding: &Binding) -> Option<Resolved> {
        match term {
            PredicateTerm::Predicate(predicate) => Some(
                self.predicate_token(predicate.as_str())
                    .map_or(Resolved::Missing, |token| Resolved::Value(u64::from(token))),
            ),
            PredicateTerm::Variable(variable) => match binding.get(variable.as_str()) {
                Some(value) => value
                    .as_predicate()
                    .map(|predicate| {
                        self.predicate_token(predicate.as_str())
                            .map_or(Resolved::Missing, |token| Resolved::Value(u64::from(token)))
                    })
                    .or(Some(Resolved::Missing)),
                None => Some(Resolved::Unbound),
            },
        }
    }

    fn atom_cell_for_query(&self, atom: &Atom) -> Resolved {
        match atom {
            Atom::Entity(entity) if entity.value() < VM_LITERAL_BASE => {
                Resolved::Value(entity.value())
            }
            Atom::Entity(_) => Resolved::Missing,
            Atom::Literal(literal) => self
                .literal_token(literal.as_str())
                .map_or(Resolved::Missing, |token| {
                    Resolved::Value(VM_LITERAL_BASE + u64::from(token))
                }),
        }
    }

    fn index_range(&self, section: usize, key: [u64; 3], length: usize) -> (usize, usize) {
        let count = self.index_count(section);
        let lower = self.lower_bound(section, key, length, count);
        let mut upper_key = key;
        if length == 0 {
            return (0, count);
        }
        let position = length - 1;
        if upper_key[position] == u64::MAX {
            return (lower, count);
        }
        upper_key[position] += 1;
        let upper = self.lower_bound(section, upper_key, length, count);
        (lower, upper)
    }

    fn lower_bound(&self, section: usize, key: [u64; 3], length: usize, count: usize) -> usize {
        let mut low = 0usize;
        let mut high = count;
        while low < high {
            let middle = low + (high - low) / 2;
            let entry = self.index_entry(section, middle).expect("validated index");
            let candidate = [entry.first, entry.second, entry.third];
            if candidate[..length] < key[..length] {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    fn unify(&self, pattern: &Pattern, record: MappedRecord, binding: &Binding) -> Option<Binding> {
        let binding = self.unify_term(&pattern.subject, record.subject, binding)?;
        let binding = self.unify_predicate(&pattern.predicate, record.predicate, &binding)?;
        self.unify_term(&pattern.object, record.object, &binding)
    }

    fn unify_term(&self, term: &Term, cell: u64, binding: &Binding) -> Option<Binding> {
        let value = BoundValue::from(self.atom_value(cell).ok()?);
        match term {
            Term::Atom(atom) if value.as_atom().as_ref() == Some(atom) => Some(binding.clone()),
            Term::Atom(_) => None,
            Term::Variable(variable) => mapped_unify_variable(variable.as_str(), value, binding),
        }
    }

    fn unify_predicate(
        &self,
        term: &PredicateTerm,
        token: u32,
        binding: &Binding,
    ) -> Option<Binding> {
        let value = BoundValue::Predicate(self.predicate_value(token as usize)?);
        match term {
            PredicateTerm::Predicate(predicate)
                if value.as_predicate().as_ref() == Some(predicate) =>
            {
                Some(binding.clone())
            }
            PredicateTerm::Predicate(_) => None,
            PredicateTerm::Variable(variable) => {
                mapped_unify_variable(variable.as_str(), value, binding)
            }
        }
    }

    pub(crate) fn frames(&self) -> Vec<Arc<CommitFrame>> {
        self.decoded_frames
            .get_or_init(|| {
                decode_frames(&self.mapping[self.sections[FRAMES].clone()])
                    .expect("validated mapped history must decode")
            })
            .clone()
    }
}

impl FrameSource for MmapVmSnapshot {
    fn frames(&self) -> Vec<Arc<CommitFrame>> {
        self.frames()
    }
}

#[derive(Clone)]
struct MappedQueryFrame {
    binding: Binding,
    provenance: Vec<SlotId>,
}

#[derive(Clone, Copy)]
enum Resolved {
    Unbound,
    Missing,
    Value(u64),
}

enum CandidateSpec {
    All,
    Empty,
    Range(usize, [u64; 3], usize),
}

fn mapped_unify_variable(name: &str, value: BoundValue, binding: &Binding) -> Option<Binding> {
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

fn collect_atom_string(atom: &Atom, literals: &mut BTreeSet<String>) {
    if let Atom::Literal(value) = atom {
        literals.insert(value.as_str().to_owned());
    }
}

fn atom_cell(atom: &Atom, literals: &BTreeMap<&str, u32>) -> Result<u64, String> {
    match atom {
        Atom::Entity(entity) if entity.value() < VM_LITERAL_BASE => Ok(entity.value()),
        Atom::Entity(_) => Err("entity overlaps physical snapshot literal namespace".to_owned()),
        Atom::Literal(literal) => literals
            .get(literal.as_str())
            .map(|token| VM_LITERAL_BASE + u64::from(*token))
            .ok_or_else(|| "literal is missing from physical snapshot dictionary".to_owned()),
    }
}

fn encode_string_table(values: &[String]) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend_from_slice(&(values.len() as u32).to_le_bytes());
    output.extend_from_slice(&0u32.to_le_bytes());
    let mut offset = 0u64;
    output.extend_from_slice(&offset.to_le_bytes());
    for value in values {
        offset += value.len() as u64;
        output.extend_from_slice(&offset.to_le_bytes());
    }
    for value in values {
        output.extend_from_slice(value.as_bytes());
    }
    output
}

fn encode_records(records: &[MappedRecord]) -> Vec<u8> {
    let mut output = vec![0u8; records.len() * RECORD_LEN];
    for (index, record) in records.iter().enumerate() {
        let start = index * RECORD_LEN;
        put_u64(&mut output, start, record.original_id);
        put_u32(&mut output, start + 8, record.slot);
        put_u32(&mut output, start + 12, record.previous);
        put_u64(&mut output, start + 16, record.subject);
        put_u32(&mut output, start + 24, record.predicate);
        put_u64(&mut output, start + 32, record.object);
    }
    output
}

fn encode_u32s(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn encode_indexes(entries: &[IndexEntry]) -> Vec<u8> {
    let mut output = vec![0u8; entries.len() * INDEX_ENTRY_LEN];
    for (index, entry) in entries.iter().enumerate() {
        let start = index * INDEX_ENTRY_LEN;
        put_u64(&mut output, start, entry.first);
        put_u64(&mut output, start + 8, entry.second);
        put_u64(&mut output, start + 16, entry.third);
        put_u32(&mut output, start + 24, entry.record);
    }
    output
}

fn encode_frames(frames: &[Arc<CommitFrame>]) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    output.extend_from_slice(&(frames.len() as u64).to_le_bytes());
    for frame in frames {
        output.extend_from_slice(&frame.parent_world().value().to_le_bytes());
        output.extend_from_slice(&frame.resulting_world().value().to_le_bytes());
        output.extend_from_slice(&frame.parent_version().to_le_bytes());
        output.extend_from_slice(&frame.resulting_version().to_le_bytes());
        output.extend_from_slice(&frame.resulting_allocator().to_le_bytes());
        let count = u32::try_from(frame.operations().len())
            .map_err(|_| "snapshot frame has too many operations".to_owned())?;
        output.extend_from_slice(&count.to_le_bytes());
        for operation in frame.operations() {
            encode_operation(operation, &mut output)?;
        }
    }
    Ok(output)
}

fn encode_operation(operation: &Operation, output: &mut Vec<u8>) -> Result<(), String> {
    match operation {
        Operation::AllocateEntity { entity } => {
            output.push(0);
            output.extend_from_slice(&entity.value().to_le_bytes());
        }
        Operation::Define { slot, fact } => {
            output.push(1);
            encode_string(slot.as_str(), output)?;
            encode_atom(&fact.subject, output)?;
            encode_string(fact.predicate.as_str(), output)?;
            encode_atom(&fact.object, output)?;
        }
        Operation::Forget { slot } => {
            output.push(2);
            encode_string(slot.as_str(), output)?;
        }
    }
    Ok(())
}

fn encode_atom(atom: &Atom, output: &mut Vec<u8>) -> Result<(), String> {
    match atom {
        Atom::Entity(entity) => {
            output.push(0);
            output.extend_from_slice(&entity.value().to_le_bytes());
        }
        Atom::Literal(literal) => {
            output.push(1);
            encode_string(literal.as_str(), output)?;
        }
    }
    Ok(())
}

fn encode_string(value: &str, output: &mut Vec<u8>) -> Result<(), String> {
    let length = u32::try_from(value.len()).map_err(|_| "snapshot string too long".to_owned())?;
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn decode_frames(bytes: &[u8]) -> Result<Vec<Arc<CommitFrame>>, String> {
    let mut cursor = Cursor::new(bytes);
    let count = read_cursor_u64(&mut cursor)? as usize;
    let mut frames = Vec::with_capacity(count.min(1_000_000));
    for _ in 0..count {
        let parent_world = WorldId::new(read_cursor_u64(&mut cursor)?);
        let resulting_world = WorldId::new(read_cursor_u64(&mut cursor)?);
        let parent_version = read_cursor_u64(&mut cursor)?;
        let resulting_version = read_cursor_u64(&mut cursor)?;
        let resulting_allocator = read_cursor_u64(&mut cursor)?;
        let operation_count = read_cursor_u32(&mut cursor)? as usize;
        let mut operations = Vec::with_capacity(operation_count.min(1_000_000));
        for _ in 0..operation_count {
            operations.push(decode_operation(&mut cursor)?);
        }
        frames.push(Arc::new(CommitFrame {
            parent_world,
            resulting_world,
            parent_version,
            resulting_version,
            resulting_allocator,
            operations: Arc::from(operations),
        }));
    }
    if cursor.position() as usize != bytes.len() {
        return Err("trailing mapped history bytes".to_owned());
    }
    Ok(frames)
}

fn decode_operation(cursor: &mut Cursor<&[u8]>) -> Result<Operation, String> {
    match read_cursor_u8(cursor)? {
        0 => Ok(Operation::AllocateEntity {
            entity: EntityId::new(read_cursor_u64(cursor)?),
        }),
        1 => Ok(Operation::Define {
            slot: SlotId::new(read_cursor_string(cursor)?),
            fact: Fact::new(
                decode_atom(cursor)?,
                Predicate::new(read_cursor_string(cursor)?),
                decode_atom(cursor)?,
            ),
        }),
        2 => Ok(Operation::Forget {
            slot: SlotId::new(read_cursor_string(cursor)?),
        }),
        tag => Err(format!("unknown mapped history operation {tag}")),
    }
}

fn decode_atom(cursor: &mut Cursor<&[u8]>) -> Result<Atom, String> {
    match read_cursor_u8(cursor)? {
        0 => Ok(Atom::Entity(EntityId::new(read_cursor_u64(cursor)?))),
        1 => Ok(Atom::Literal(Literal::new(read_cursor_string(cursor)?))),
        tag => Err(format!("unknown mapped history atom {tag}")),
    }
}

fn read_cursor_string(cursor: &mut Cursor<&[u8]>) -> Result<String, String> {
    let length = read_cursor_u32(cursor)? as usize;
    let start = cursor.position() as usize;
    let end = start
        .checked_add(length)
        .ok_or_else(|| "mapped history string overflow".to_owned())?;
    let bytes = cursor
        .get_ref()
        .get(start..end)
        .ok_or_else(|| "truncated mapped history string".to_owned())?;
    let value = std::str::from_utf8(bytes)
        .map_err(|error| error.to_string())?
        .to_owned();
    cursor.set_position(end as u64);
    Ok(value)
}

fn read_cursor_u8(cursor: &mut Cursor<&[u8]>) -> Result<u8, String> {
    let mut bytes = [0; 1];
    cursor
        .read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(bytes[0])
}

fn read_cursor_u32(cursor: &mut Cursor<&[u8]>) -> Result<u32, String> {
    let mut bytes = [0; 4];
    cursor
        .read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_cursor_u64(cursor: &mut Cursor<&[u8]>) -> Result<u64, String> {
    let mut bytes = [0; 8];
    cursor
        .read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(u64::from_le_bytes(bytes))
}

fn align_eight(bytes: &mut Vec<u8>) {
    while bytes.len() % 8 != 0 {
        bytes.push(0);
    }
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_u32_at(bytes: &[u8], offset: usize) -> Result<u32, String> {
    bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "truncated u32".to_owned())?
        .try_into()
        .map(u32::from_le_bytes)
        .map_err(|_| "invalid u32".to_owned())
}

fn read_u64_at(bytes: &[u8], offset: usize) -> Result<u64, String> {
    bytes
        .get(offset..offset + 8)
        .ok_or_else(|| "truncated u64".to_owned())?
        .try_into()
        .map(u64::from_le_bytes)
        .map_err(|_| "invalid u64".to_owned())
}

fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(CHECKSUM_OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(CHECKSUM_PRIME)
    })
}
