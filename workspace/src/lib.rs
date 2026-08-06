#![deny(unsafe_op_in_unsafe_fn)]

mod ffi;

use std::cell::UnsafeCell;
use std::hint::spin_loop;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

const ABI_VERSION: u32 = 1;
const STATUS_OK: u32 = 0;
const STATUS_FULL: u32 = 1;
const STATUS_EMPTY: u32 = 2;
const STATUS_INVALID: u32 = 3;
const STATUS_INTERNAL_ERROR: u32 = 4;

const INLINE_VALUE_CAPACITY: usize = 8;
const PUBLICATION_SHARD_COUNT: usize = 2;
const LOCK_SPIN_ATTEMPTS: usize = 4096;
const LOCK_YIELD_ATTEMPTS: usize = 1;

enum SlotStorage {
    Inline {
        bytes: [u8; INLINE_VALUE_CAPACITY],
        length: usize,
    },
    Heap(Vec<u8>),
}

struct Slot {
    storage: SlotStorage,
}

impl Slot {
    fn new(max_value_size: usize) -> Self {
        let storage = if max_value_size <= INLINE_VALUE_CAPACITY {
            SlotStorage::Inline {
                bytes: [0; INLINE_VALUE_CAPACITY],
                length: 0,
            }
        } else {
            // Keep the larger-value path lazy: the reusable Vec obtains its
            // backing allocation only when a value is first copied into it.
            SlotStorage::Heap(Vec::new())
        };
        Self { storage }
    }

    fn copy_value(&mut self, data: &[u8]) -> Result<(), ()> {
        match &mut self.storage {
            SlotStorage::Inline { bytes, length } => {
                debug_assert!(data.len() <= INLINE_VALUE_CAPACITY);
                bytes[..data.len()].copy_from_slice(data);
                *length = data.len();
                Ok(())
            }
            SlotStorage::Heap(value) => {
                value.clear();
                if value.capacity() < data.len() {
                    value.try_reserve_exact(data.len()).map_err(|_| ())?;
                }
                value.extend_from_slice(data);
                Ok(())
            }
        }
    }

    #[inline]
    fn len(&self) -> usize {
        match &self.storage {
            SlotStorage::Inline { length, .. } => *length,
            SlotStorage::Heap(value) => value.len(),
        }
    }

    #[inline]
    fn copy_to(&self, output: &mut [u8]) {
        match &self.storage {
            SlotStorage::Inline { bytes, length } => {
                debug_assert_eq!(*length, output.len());
                output.copy_from_slice(&bytes[..*length]);
            }
            SlotStorage::Heap(value) => {
                debug_assert_eq!(value.len(), output.len());
                output.copy_from_slice(value);
            }
        }
    }

    fn clear(&mut self) {
        match &mut self.storage {
            SlotStorage::Inline { length, .. } => *length = 0,
            SlotStorage::Heap(value) => value.clear(),
        }
    }
}

#[repr(align(64))]
struct SpinMutex<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

struct SpinMutexGuard<'a, T> {
    mutex: &'a SpinMutex<T>,
}

impl<T> SpinMutex<T> {
    fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    #[inline]
    fn lock(&self) -> SpinMutexGuard<'_, T> {
        loop {
            if self
                .locked
                .compare_exchange_weak(
                    false,
                    true,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return SpinMutexGuard { mutex: self };
            }
            for _ in 0..LOCK_SPIN_ATTEMPTS {
                spin_loop();
            }

            for _ in 0..LOCK_YIELD_ATTEMPTS {
                if !self.locked.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::yield_now();
            }
        }
    }
}

unsafe impl<T: Send> Sync for SpinMutex<T> {}
unsafe impl<T: Send> Send for SpinMutex<T> {}

impl<T> Deref for SpinMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for SpinMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for SpinMutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.locked.store(false, Ordering::Release);
    }
}

struct SlotIndexHeap {
    entries: Vec<PublishedEntry>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PublishedEntry {
    priority: u64,
    slot_index: usize,
}

impl SlotIndexHeap {
    fn new(capacity: usize) -> Result<Self, u32> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;
        Ok(Self { entries })
    }

    #[inline]
    fn peek(&self) -> Option<PublishedEntry> {
        self.entries.first().copied()
    }

    #[inline]
    fn push(&mut self, entry: PublishedEntry) {
        self.entries.push(entry);
        let mut child = self.entries.len() - 1;
        while child != 0 {
            let parent = (child - 1) / 2;
            if !comes_before(self.entries[child], self.entries[parent]) {
                break;
            }
            self.entries.swap(child, parent);
            child = parent;
        }
    }

    #[inline]
    fn pop(&mut self) -> Option<PublishedEntry> {
        let root = self.entries.first().copied()?;
        let last = self.entries.pop().expect("heap root implies a last item");
        if !self.entries.is_empty() {
            self.entries[0] = last;
            let mut parent = 0;
            loop {
                let left = parent * 2 + 1;
                if left >= self.entries.len() {
                    break;
                }
                let right = left + 1;
                let mut child = left;
                if right < self.entries.len()
                    && comes_before(self.entries[right], self.entries[left])
                {
                    child = right;
                }
                if !comes_before(self.entries[child], self.entries[parent]) {
                    break;
                }
                self.entries.swap(parent, child);
                parent = child;
            }
        }
        Some(root)
    }
}

#[inline]
fn comes_before(left: PublishedEntry, right: PublishedEntry) -> bool {
    left.priority < right.priority
        || (left.priority == right.priority && left.slot_index < right.slot_index)
}

#[inline]
fn root_priority(entry: Option<PublishedEntry>) -> Option<u64> {
    entry.map(|entry| entry.priority)
}

struct ReservationState {
    free_slots: Vec<usize>,
}

struct PublishedState {
    heap: SlotIndexHeap,
}

#[derive(Clone, Copy)]
struct RootObservation {
    priority: u64,
    sequence: u64,
}

#[derive(Clone, Copy)]
struct RootCache {
    priority: u64,
    sequence: u64,
    present: bool,
}

// A root has one more possible state than a u64 priority (empty plus every
// possible priority, including u64::MAX). Keep the state in two atomics, but
// publish them through a sequence counter. Readers accept a pair only when
// they observed the same even sequence number before and after both fields.
// Writers are serialized by the shard lock, while readers do not need that
// lock just to inspect a root.
#[repr(align(64))]
struct RootSummary {
    sequence: AtomicU64,
    priority: AtomicU64,
}

const ROOT_PRESENT_BIT: u64 = 1 << 63;
const ROOT_VERSION_MASK: u64 = ROOT_PRESENT_BIT - 1;

impl RootSummary {
    fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            priority: AtomicU64::new(u64::MAX),
        }
    }

    fn publish(&self, entry: Option<PublishedEntry>) {
        // The odd sequence marks the two fields as transient. The even
        // sequence after the stores makes one complete root state visible.
        let current = self.sequence.load(Ordering::Relaxed);
        let version = current & ROOT_VERSION_MASK;
        self.sequence.store(version | 1, Ordering::Release);
        self.priority
            .store(entry.map_or(u64::MAX, |entry| entry.priority), Ordering::Relaxed);
        let next_version = version.wrapping_add(2) & ROOT_VERSION_MASK;
        self.sequence.store(
            next_version | if entry.is_some() { ROOT_PRESENT_BIT } else { 0 },
            Ordering::Release,
        );
    }

    #[inline(always)]
    fn read(&self) -> RootCache {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                spin_loop();
                continue;
            }

            if before & ROOT_PRESENT_BIT == 0 {
                let after = self.sequence.load(Ordering::Acquire);
                if before == after {
                    return RootCache {
                        priority: u64::MAX,
                        sequence: after,
                        present: false,
                    };
                }
                continue;
            }

            let priority = self.priority.load(Ordering::Relaxed);
            let after = self.sequence.load(Ordering::Acquire);
            if before == after && after & 1 == 0 {
                return RootCache {
                    priority,
                    sequence: after,
                    present: after & ROOT_PRESENT_BIT != 0,
                };
            }
        }
    }

    #[inline(always)]
    fn observe(&self) -> Option<RootObservation> {
        let root = self.read();
        root.present.then_some(RootObservation {
            priority: root.priority,
            sequence: root.sequence,
        })
    }
}

// Exact count of nonempty publication shards. It is updated only on an
// empty/nonempty transition, so the apparent-empty dequeue path can validate
// emptiness without taking every shard lock.
#[repr(align(64))]
struct PublishedShardCount {
    value: AtomicUsize,
}

impl PublishedShardCount {
    fn new() -> Self {
        Self {
            value: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn load(&self) -> usize {
        self.value.load(Ordering::Acquire)
    }

    #[inline]
    fn increment(&self) {
        self.value.fetch_add(1, Ordering::Release);
    }

    #[inline]
    fn decrement(&self) {
        self.value.fetch_sub(1, Ordering::Release);
    }
}

#[repr(align(64))]
struct PublicationShard {
    published: SpinMutex<PublishedState>,
    root: RootSummary,
}

impl PublicationShard {
    fn new(capacity: usize) -> Result<Self, u32> {
        Ok(Self {
            published: SpinMutex::new(PublishedState {
                heap: SlotIndexHeap::new(capacity)?,
            }),
            root: RootSummary::new(),
        })
    }
}

struct QueueState {
    max_value_size: usize,
    slots: Vec<UnsafeCell<Slot>>,
    reservation: SpinMutex<ReservationState>,
    published_shard_count: PublishedShardCount,
    publication_shards: Vec<PublicationShard>,
}

// A slot is owned by exactly one phase at a time:
//
//   free-slot stack -> reserving producer -> publication heap -> removing consumer
//   -> free-slot stack
//
// The reservation and publication mutexes protect those transitions. A producer
// writes a reserved slot before publishing its index, and a consumer reads a
// removed slot before recycling its index. Thus no two threads access a slot's
// payload concurrently even though the payload itself is not mutex-protected.
unsafe impl Sync for QueueState {}

impl QueueState {
    unsafe fn slot(&self, slot_index: usize) -> &Slot {
        unsafe { &*self.slots[slot_index].get() }
    }

    unsafe fn slot_mut(&self, slot_index: usize) -> &mut Slot {
        unsafe { &mut *self.slots[slot_index].get() }
    }
}

pub struct Queue {
    producer_count: u32,
    consumer_count: u32,
    state: Arc<QueueState>,
}

impl Queue {
    fn new(
        capacity: u64,
        max_value_size: u64,
        producer_count: u32,
        consumer_count: u32,
    ) -> Result<Self, u32> {
        if capacity == 0 || max_value_size == 0 || producer_count == 0 || consumer_count == 0 {
            return Err(STATUS_INVALID);
        }
        let capacity = usize::try_from(capacity).map_err(|_| STATUS_INVALID)?;
        let max_value_size = usize::try_from(max_value_size).map_err(|_| STATUS_INVALID)?;
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;
        for _ in 0..capacity {
            slots.push(UnsafeCell::new(Slot::new(max_value_size)));
        }

        let mut free_slots = Vec::new();
        free_slots
            .try_reserve_exact(capacity)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;
        for index in (0..capacity).rev() {
            free_slots.push(index);
        }

        let shard_count = capacity.min(PUBLICATION_SHARD_COUNT);
        let shard_capacity = capacity / shard_count;
        let extra_shards = capacity % shard_count;
        let mut publication_shards = Vec::new();
        publication_shards
            .try_reserve_exact(shard_count)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;
        for shard_index in 0..shard_count {
            let capacity = shard_capacity + usize::from(shard_index < extra_shards);
            publication_shards.push(PublicationShard::new(capacity)?);
        }

        Ok(Self {
            producer_count,
            consumer_count,
            state: Arc::new(QueueState {
                max_value_size,
                slots,
                reservation: SpinMutex::new(ReservationState { free_slots }),
                published_shard_count: PublishedShardCount::new(),
                publication_shards,
            }),
        })
    }

    fn producer(&self, id: u32) -> Result<Producer, u32> {
        (id < self.producer_count)
            .then(|| Producer {
                state: Arc::clone(&self.state),
            })
            .ok_or(STATUS_INVALID)
    }

    fn consumer(&self, id: u32) -> Result<Consumer, u32> {
        (id < self.consumer_count)
            .then(|| Consumer {
                state: Arc::clone(&self.state),
                shard_start: ((id as usize) + 1) % self.state.publication_shards.len(),
            })
            .ok_or(STATUS_INVALID)
    }
}

pub struct Producer {
    state: Arc<QueueState>,
}

impl Producer {
    fn try_enqueue(&self, priority: u64, data: &[u8]) -> u32 {
        if data.len() > self.state.max_value_size {
            return STATUS_INVALID;
        }
        let slot_index = {
            let mut reservation = self.state.reservation.lock();
            let Some(slot_index) = reservation.free_slots.pop() else {
                return STATUS_FULL;
            };
            slot_index
        };

        // The reservation gives this producer exclusive access to the slot.
        // Keep the borrowed input copy independent from both shared queue locks.
        if unsafe { self.state.slot_mut(slot_index) }
            .copy_value(data)
            .is_err()
        {
            let mut reservation = self.state.reservation.lock();
            reservation.free_slots.push(slot_index);
            return STATUS_INTERNAL_ERROR;
        }
        {
            let shard_index = slot_index % self.state.publication_shards.len();
            let shard = &self.state.publication_shards[shard_index];
            let mut published = shard.published.lock();

            // Publication is short-held and only touches heap/index metadata.
            // The payload was fully copied before this lock was acquired.
            let previous_root = root_priority(published.heap.peek());
            published.heap.push(PublishedEntry {
                priority,
                slot_index,
            });
            let next_root = published.heap.peek();
            if root_priority(next_root) != previous_root {
                shard.root.publish(next_root);
            }
            if previous_root.is_none() {
                self.state.published_shard_count.increment();
            }
        }
        STATUS_OK
    }
}

pub struct Consumer {
    state: Arc<QueueState>,
    shard_start: usize,
}

impl Consumer {
    fn try_dequeue(&self, output: &mut [u8]) -> Result<usize, u32> {
        let (slot_index, length) = loop {
            let mut observations: [Option<RootObservation>; PUBLICATION_SHARD_COUNT] =
                [None; PUBLICATION_SHARD_COUNT];
            let mut selected: Option<(usize, RootObservation)> = None;
            let shard_count = self.state.publication_shards.len();
            for offset in 0..shard_count {
                let shard_index = (self.shard_start + offset) % shard_count;
                let shard = &self.state.publication_shards[shard_index];
                let Some(observation) = shard.root.observe() else {
                    continue;
                };
                observations[shard_index] = Some(observation);
                if selected.as_ref().map_or(true, |(_, current)| {
                    observation.priority < current.priority
                }) {
                    selected = Some((shard_index, observation));
                }
            }
            let Some((shard_index, _observation)) = selected else {
                // Root summaries are optimistic reads. Confirm an apparent
                // empty queue against every exact heap before returning EMPTY.
                if self.confirm_empty() {
                    return Err(STATUS_EMPTY);
                }
                continue;
            };

            let shard = &self.state.publication_shards[shard_index];
            let mut published = shard.published.lock();
            let Some(current) = published.heap.peek() else {
                continue;
            };

            // The selected heap root must still match the observation. A
            // changed root means a concurrent operation changed this shard,
            // so restart the global selection before touching the payload.
            if observations[shard_index]
                .is_none_or(|observation| observation.priority != current.priority)
            {
                continue;
            }

            // Re-read every other shard after acquiring the selected lock.
            // The selected heap is exact under its lock. A synchronized root
            // observation that is lower than the selected root invalidates
            // this attempt; changes after these reads overlap this dequeue
            // and may be ordered after it.
            let another_shard_has_lower_root = self
                .state
                .publication_shards
                .iter()
                .enumerate()
                .any(|(candidate_index, candidate_shard)| {
                    if candidate_index == shard_index {
                        return false;
                    }
                    let Some(observation) = observations[candidate_index] else {
                        return candidate_shard
                            .root
                            .observe()
                            .is_some_and(|candidate_root| {
                                candidate_root.priority < current.priority
                            });
                    };
                    if candidate_shard.root.sequence.load(Ordering::Acquire) == observation.sequence
                    {
                        return false;
                    }
                    candidate_shard
                        .root
                        .observe()
                        .is_some_and(|candidate_root| candidate_root.priority < current.priority)
                });
            if another_shard_has_lower_root {
                continue;
            }

            let length = unsafe { self.state.slot(current.slot_index) }.len();
            if length > output.len() {
                return Err(STATUS_INVALID);
            }
            // Removing the index transfers exclusive slot ownership to this
            // consumer. Do not hold publication while copying the payload.
            let entry = published
                .heap
                .pop()
                .expect("heap peek implies a heap item");
            let next_root = published.heap.peek();
            if root_priority(next_root) != Some(entry.priority) {
                shard.root.publish(next_root);
            }
            if next_root.is_none() {
                self.state.published_shard_count.decrement();
            }
            break (entry.slot_index, length);
        };

        {
            let slot = unsafe { self.state.slot(slot_index) };
            slot.copy_to(&mut output[..length]);
        }

        // The consumer still owns the slot while clearing it. Only then make
        // it visible to a producer again.
        unsafe { self.state.slot_mut(slot_index) }.clear();
        let mut reservation = self.state.reservation.lock();
        reservation.free_slots.push(slot_index);
        Ok(length)
    }

    fn confirm_empty(&self) -> bool {
        self.state.published_shard_count.load() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_core_is_bounded_priority_queue() {
        let queue = Queue::new(3, 8, 1, 1).unwrap();
        let producer = queue.producer(0).unwrap();
        let consumer = queue.consumer(0).unwrap();
        assert_eq!(producer.try_enqueue(7, b"later"), STATUS_OK);
        assert_eq!(producer.try_enqueue(2, b"first"), STATUS_OK);
        assert_eq!(producer.try_enqueue(2, b"second"), STATUS_OK);
        assert_eq!(producer.try_enqueue(0, b"full"), STATUS_FULL);

        let mut output = [0_u8; 8];
        for expected in [b"first".as_slice(), b"second", b"later"] {
            assert_eq!(consumer.try_dequeue(&mut output), Ok(expected.len()));
            assert_eq!(&output[..expected.len()], expected);
        }
        assert_eq!(consumer.try_dequeue(&mut output), Err(STATUS_EMPTY));
    }

    #[test]
    fn maximum_priority_is_distinct_from_empty() {
        let root = RootSummary::new();
        assert!(root.observe().is_none());
        root.publish(Some(PublishedEntry {
            priority: u64::MAX,
            slot_index: 0,
        }));
        assert_eq!(
            root.observe().map(|observation| observation.priority),
            Some(u64::MAX)
        );
        root.publish(None);
        assert!(root.observe().is_none());

        let queue = Queue::new(2, 1, 1, 1).unwrap();
        let producer = queue.producer(0).unwrap();
        let consumer = queue.consumer(0).unwrap();
        assert_eq!(producer.try_enqueue(u64::MAX, &[0x5a]), STATUS_OK);
        let mut output = [0_u8; 1];
        assert_eq!(consumer.try_dequeue(&mut output), Ok(1));
        assert_eq!(output, [0x5a]);
        assert_eq!(consumer.try_dequeue(&mut output), Err(STATUS_EMPTY));
    }

    #[test]
    fn reusable_slots_copy_empty_and_non_word_aligned_values() {
        let queue = Queue::new(3, 9, 1, 1).unwrap();
        let producer = queue.producer(0).unwrap();
        let consumer = queue.consumer(0).unwrap();

        assert_eq!(producer.try_enqueue(3, &[]), STATUS_OK);
        assert_eq!(producer.try_enqueue(2, &[0x11; 7]), STATUS_OK);
        assert_eq!(producer.try_enqueue(1, &[0x22; 9]), STATUS_OK);

        let mut empty = [];
        assert_eq!(consumer.try_dequeue(&mut empty), Err(STATUS_INVALID));

        let mut output = [0_u8; 9];
        assert_eq!(consumer.try_dequeue(&mut output), Ok(9));
        assert_eq!(output, [0x22; 9]);
        assert_eq!(consumer.try_dequeue(&mut output), Ok(7));
        assert_eq!(&output[..7], &[0x11; 7]);
        assert_eq!(consumer.try_dequeue(&mut output), Ok(0));
    }

    #[test]
    fn inline_slots_copy_every_supported_length() {
        for max_value_size in 1..=INLINE_VALUE_CAPACITY {
            let mut slot = Slot::new(max_value_size);
            assert!(matches!(slot.storage, SlotStorage::Inline { .. }));

            for length in 0..=max_value_size {
                let input = vec![length as u8; length];
                slot.copy_value(&input).unwrap();
                assert_eq!(slot.len(), length);

                let mut output = vec![0_u8; length];
                slot.copy_to(&mut output);
                assert_eq!(output, input);
                slot.clear();
                assert_eq!(slot.len(), 0);
            }
        }
    }

    #[test]
    fn heap_slots_start_lazy_and_reuse_storage() {
        let mut slot = Slot::new(INLINE_VALUE_CAPACITY + 1);
        assert!(matches!(&slot.storage, SlotStorage::Heap(value) if value.capacity() == 0));

        slot.copy_value(&[0x11; 9]).unwrap();
        let capacity = match &slot.storage {
            SlotStorage::Heap(value) => value.capacity(),
            SlotStorage::Inline { .. } => unreachable!(),
        };
        assert!(capacity >= 9);
        slot.clear();
        slot.copy_value(&[0x22; 7]).unwrap();
        assert_eq!(slot.len(), 7);
        let mut output = [0_u8; 7];
        slot.copy_to(&mut output);
        assert_eq!(output, [0x22; 7]);
    }
}
