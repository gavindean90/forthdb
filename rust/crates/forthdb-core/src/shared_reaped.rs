mod engine {
    include!("shared_v4.rs");
}

use crate::{
    BoundValue, EntityId, Fact, Pattern, Predicate, QueryOptions, QueryResult, Record, RecordId,
    SlotId, SourceTerm, Symbol,
};
use crossbeam_queue::{ArrayQueue, SegQueue};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

pub use engine::StructuralMetrics;

const REAPER_QUEUE_CAPACITY: usize = 4096;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReaperMetrics {
    pub queued_roots: usize,
    pub retired_roots: u64,
    pub reaped_roots: u64,
    pub overflow_enqueues: u64,
    pub worker_alive: bool,
}

struct ReaperCounters {
    queued: AtomicUsize,
    retired: AtomicU64,
    reaped: AtomicU64,
    overflow: AtomicU64,
    alive: AtomicBool,
}

impl ReaperCounters {
    fn metrics(&self) -> ReaperMetrics {
        ReaperMetrics {
            queued_roots: self.queued.load(Ordering::Acquire),
            retired_roots: self.retired.load(Ordering::Relaxed),
            reaped_roots: self.reaped.load(Ordering::Relaxed),
            overflow_enqueues: self.overflow.load(Ordering::Relaxed),
            worker_alive: self.alive.load(Ordering::Acquire),
        }
    }
}

struct KernelReaper {
    queue: Arc<ArrayQueue<engine::ForthDb>>,
    overflow: Arc<SegQueue<engine::ForthDb>>,
    wake: mpsc::SyncSender<()>,
    counters: Arc<ReaperCounters>,
}

impl KernelReaper {
    fn new() -> Self {
        let queue = Arc::new(ArrayQueue::new(REAPER_QUEUE_CAPACITY));
        let overflow = Arc::new(SegQueue::new());
        let counters = Arc::new(ReaperCounters {
            queued: AtomicUsize::new(0),
            retired: AtomicU64::new(0),
            reaped: AtomicU64::new(0),
            overflow: AtomicU64::new(0),
            alive: AtomicBool::new(true),
        });
        let (wake, receiver) = mpsc::sync_channel(1);

        let worker_queue = queue.clone();
        let worker_overflow = overflow.clone();
        let worker_counters = counters.clone();
        thread::Builder::new()
            .name("forthdb-world-reaper".to_owned())
            .spawn(move || loop {
                let _ = receiver.recv_timeout(Duration::from_millis(10));
                drain_available(&worker_queue, &worker_overflow, &worker_counters);
            })
            .expect("ForthDB world reaper thread must start");

        Self {
            queue,
            overflow,
            wake,
            counters,
        }
    }

    fn retire(&self, kernel: engine::ForthDb) {
        self.counters.retired.fetch_add(1, Ordering::Relaxed);
        self.counters.queued.fetch_add(1, Ordering::Release);
        if let Err(kernel) = self.queue.push(kernel) {
            self.counters.overflow.fetch_add(1, Ordering::Relaxed);
            self.overflow.push(kernel);
        }
        let _ = self.wake.try_send(());
    }

    fn metrics(&self) -> ReaperMetrics {
        self.counters.metrics()
    }

    fn drain(&self, timeout: Duration) -> bool {
        let started = Instant::now();
        let _ = self.wake.try_send(());
        while self.counters.queued.load(Ordering::Acquire) != 0 {
            if started.elapsed() >= timeout {
                return false;
            }
            thread::sleep(Duration::from_micros(100));
            let _ = self.wake.try_send(());
        }
        true
    }
}

fn drain_available(
    queue: &ArrayQueue<engine::ForthDb>,
    overflow: &SegQueue<engine::ForthDb>,
    counters: &ReaperCounters,
) {
    while let Some(kernel) = queue.pop() {
        drop(kernel);
        counters.reaped.fetch_add(1, Ordering::Relaxed);
        counters.queued.fetch_sub(1, Ordering::Release);
    }

    while let Some(kernel) = overflow.pop() {
        drop(kernel);
        counters.reaped.fetch_add(1, Ordering::Relaxed);
        counters.queued.fetch_sub(1, Ordering::Release);
    }
}

fn reaper() -> &'static KernelReaper {
    static REAPER: OnceLock<KernelReaper> = OnceLock::new();
    REAPER.get_or_init(KernelReaper::new)
}

pub struct ForthDb {
    inner: Option<engine::ForthDb>,
}

impl Clone for ForthDb {
    fn clone(&self) -> Self {
        Self {
            inner: Some(self.inner().clone()),
        }
    }
}

impl Default for ForthDb {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ForthDb {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            reaper().retire(inner);
        }
    }
}

impl ForthDb {
    fn inner(&self) -> &engine::ForthDb {
        self.inner
            .as_ref()
            .expect("ForthDB kernel is unavailable only during destruction")
    }

    fn inner_mut(&mut self) -> &mut engine::ForthDb {
        self.inner
            .as_mut()
            .expect("ForthDB kernel is unavailable only during destruction")
    }

    pub fn new() -> Self {
        Self {
            inner: Some(engine::ForthDb::new()),
        }
    }

    pub fn reaper_metrics() -> ReaperMetrics {
        reaper().metrics()
    }

    pub fn drain_reaper(timeout: Duration) -> bool {
        reaper().drain(timeout)
    }

    pub fn structural_metrics(&self) -> StructuralMetrics {
        self.inner().structural_metrics()
    }

    pub fn entity(&mut self) -> EntityId {
        self.inner_mut().entity()
    }

    pub fn define(&mut self, slot: SlotId, fact: Fact) -> RecordId {
        self.inner_mut().define(slot, fact)
    }

    pub fn forget(&mut self, slot: SlotId) -> RecordId {
        self.inner_mut().forget(slot)
    }

    pub fn resolve(&self, slot: &SlotId) -> Option<&Fact> {
        self.inner().resolve(slot)
    }

    pub fn definitions(&self, slot: &SlotId) -> Vec<&Fact> {
        self.inner().definitions(slot)
    }

    pub fn history(&self, slot: &SlotId) -> Vec<&Record> {
        self.inner().history(slot)
    }

    pub fn active_slot_count(&self) -> usize {
        self.inner().active_slot_count()
    }

    pub fn record_count(&self) -> usize {
        self.inner().record_count()
    }

    pub fn display_slot(entity: EntityId) -> SlotId {
        engine::ForthDb::display_slot(entity)
    }

    pub fn symbol_slot(namespace: &str, symbol: &Symbol) -> SlotId {
        engine::ForthDb::symbol_slot(namespace, symbol)
    }

    pub fn define_display_name(&mut self, entity: EntityId, name: impl Into<String>) -> RecordId {
        self.inner_mut().define_display_name(entity, name)
    }

    pub fn display_name(&self, entity: EntityId) -> String {
        self.inner().display_name(entity)
    }

    pub fn bind_symbol(
        &mut self,
        namespace: &str,
        symbol: Symbol,
        entity: EntityId,
    ) -> RecordId {
        self.inner_mut().bind_symbol(namespace, symbol, entity)
    }

    pub fn resolve_symbol(&self, namespace: &str, symbol: &Symbol) -> Option<EntityId> {
        self.inner().resolve_symbol(namespace, symbol)
    }

    pub fn compile_pattern(
        &self,
        namespace: &str,
        subject: SourceTerm,
        predicate: Predicate,
        object: SourceTerm,
    ) -> Result<Pattern, String> {
        self.inner()
            .compile_pattern(namespace, subject, predicate, object)
    }

    pub fn query(&self, patterns: &[Pattern], options: QueryOptions) -> QueryResult {
        self.inner().query(patterns, options)
    }

    pub fn render_value(&self, value: &BoundValue) -> String {
        self.inner().render_value(value)
    }

    pub fn validate(&self) -> Result<(), String> {
        self.inner().validate()
    }

    pub fn validate_full(&self) -> Result<(), String> {
        self.inner().validate_full()
    }
}
