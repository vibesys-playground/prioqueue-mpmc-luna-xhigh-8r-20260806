#![deny(unsafe_op_in_unsafe_fn)]

mod ffi;

use std::cell::UnsafeCell;
use std::hint::spin_loop;
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};
use std::ptr::{read_unaligned, write_unaligned};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const ABI_VERSION: u32 = 1;
const STATUS_OK: u32 = 0;
const STATUS_FULL: u32 = 1;
const STATUS_EMPTY: u32 = 2;
const STATUS_INVALID: u32 = 3;
const STATUS_INTERNAL_ERROR: u32 = 4;

const INLINE_VALUE_CAPACITY: usize = 8;

#[inline(always)]
fn copy_inline_bytes(destination: &mut [u8], source: &[u8]) {
    debug_assert_eq!(destination.len(), source.len());
    debug_assert!(source.len() <= INLINE_VALUE_CAPACITY);
    let length = source.len();
    let destination = destination.as_mut_ptr();
    let source = source.as_ptr();
    unsafe {
        match length {
            0 => {}
            1 => write_unaligned(destination, read_unaligned(source)),
            2 => write_unaligned(
                destination.cast::<u16>(),
                read_unaligned(source.cast::<u16>()),
            ),
            3 => {
                write_unaligned(
                    destination.cast::<u16>(),
                    read_unaligned(source.cast::<u16>()),
                );
                write_unaligned(destination.add(2), read_unaligned(source.add(2)));
            }
            4 => write_unaligned(
                destination.cast::<u32>(),
                read_unaligned(source.cast::<u32>()),
            ),
            5 => {
                write_unaligned(
                    destination.cast::<u32>(),
                    read_unaligned(source.cast::<u32>()),
                );
                write_unaligned(destination.add(4), read_unaligned(source.add(4)));
            }
            6 => {
                write_unaligned(
                    destination.cast::<u32>(),
                    read_unaligned(source.cast::<u32>()),
                );
                write_unaligned(
                    destination.add(4).cast::<u16>(),
                    read_unaligned(source.add(4).cast::<u16>()),
                );
            }
            7 => {
                write_unaligned(
                    destination.cast::<u32>(),
                    read_unaligned(source.cast::<u32>()),
                );
                write_unaligned(
                    destination.add(4).cast::<u16>(),
                    read_unaligned(source.add(4).cast::<u16>()),
                );
                write_unaligned(destination.add(6), read_unaligned(source.add(6)));
            }
            8 => write_unaligned(
                destination.cast::<u64>(),
                read_unaligned(source.cast::<u64>()),
            ),
            _ => unreachable!(),
        }
    }
}

enum SlotStorage {
    Inline { bytes: [u8; INLINE_VALUE_CAPACITY] },
    Heap(Vec<u8>),
}

struct Slot {
    length: usize,
    storage: SlotStorage,
}

impl Slot {
    fn new(max_value_size: usize) -> Self {
        let storage = if max_value_size <= INLINE_VALUE_CAPACITY {
            SlotStorage::Inline {
                bytes: [0; INLINE_VALUE_CAPACITY],
            }
        } else {
            // Keep the larger-value path lazy: the reusable Vec obtains its
            // backing allocation only when a value is first copied into it.
            SlotStorage::Heap(Vec::new())
        };
        Self { length: 0, storage }
    }

    fn copy_value(&mut self, data: &[u8]) -> Result<(), ()> {
        match &mut self.storage {
            SlotStorage::Inline { bytes } => {
                debug_assert!(data.len() <= INLINE_VALUE_CAPACITY);
                copy_inline_bytes(&mut bytes[..data.len()], data);
                self.length = data.len();
                Ok(())
            }
            SlotStorage::Heap(value) => {
                value.clear();
                if value.capacity() < data.len() {
                    value.try_reserve_exact(data.len()).map_err(|_| ())?;
                }
                value.extend_from_slice(data);
                self.length = data.len();
                Ok(())
            }
        }
    }

    #[inline]
    fn len(&self) -> usize {
        self.length
    }

    #[inline]
    fn copy_to(&self, output: &mut [u8]) {
        debug_assert_eq!(self.length, output.len());
        match &self.storage {
            SlotStorage::Inline { bytes } => {
                copy_inline_bytes(output, &bytes[..self.length]);
            }
            SlotStorage::Heap(value) => {
                output.copy_from_slice(value);
            }
        }
    }

    fn clear(&mut self) {
        self.length = 0;
        if let SlotStorage::Heap(value) = &mut self.storage {
            value.clear();
        }
    }
}

#[repr(align(64))]
struct SpinMutex<T> {
    locked: AtomicBool,
    _lock_padding: [u8; 63],
    value: UnsafeCell<T>,
}

struct SpinMutexGuard<'a, T> {
    mutex: &'a SpinMutex<T>,
}

impl<T> SpinMutex<T> {
    fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            _lock_padding: [0; 63],
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
            while self.locked.load(Ordering::Relaxed) {
                spin_loop();
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

// The publication lock is the queue's remaining shared hot spot. Keep its
// heap as a fixed-capacity array of the complete publication records so heap
// maintenance never has to touch slot payload storage or allocate.
struct PublishedEntryHeap {
    entries: Vec<MaybeUninit<PublishedEntry>>,
    length: usize,
}

const HEAP_ARITY: usize = 4;
const HEAP_ARITY_SHIFT: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PublishedEntry {
    priority: u64,
    slot_index: usize,
}

impl PublishedEntryHeap {
    fn new(capacity: usize) -> Result<Self, u32> {
        let mut entries = Vec::<MaybeUninit<PublishedEntry>>::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;
        // The backing vector is fixed at queue creation. Only `length` is
        // changed by heap operations; the spare slots are intentionally
        // uninitialized so queue construction does not materialize entries.
        unsafe { entries.set_len(capacity) };
        Ok(Self { entries, length: 0 })
    }

    #[inline]
    fn peek(&self) -> Option<PublishedEntry> {
        (self.length != 0).then(|| unsafe { self.entry(0) })
    }

    #[inline(always)]
    unsafe fn entry(&self, index: usize) -> PublishedEntry {
        unsafe { self.entries.get_unchecked(index).assume_init_read() }
    }

    #[inline(always)]
    unsafe fn write_entry(&mut self, index: usize, entry: PublishedEntry) {
        unsafe { self.entries.get_unchecked_mut(index).write(entry) };
    }

    #[inline]
    fn push(&mut self, entry: PublishedEntry) {
        let mut child = self.length;
        debug_assert!(child < self.entries.len());
        self.length = child + 1;
        while child != 0 {
            let parent = (child - 1) >> HEAP_ARITY_SHIFT;
            let parent_entry = unsafe { self.entry(parent) };
            if !comes_before(entry, parent_entry) {
                break;
            }
            unsafe { self.write_entry(child, parent_entry) };
            child = parent;
        }
        unsafe { self.write_entry(child, entry) };
    }

    #[inline]
    fn pop(&mut self) -> Option<PublishedEntry> {
        let length = self.length;
        if length == 0 {
            return None;
        }
        let root = unsafe { self.entry(0) };
        let new_length = length - 1;
        self.length = new_length;
        if new_length != 0 {
            let last = unsafe { self.entry(new_length) };
            let mut parent = 0;

            // In a full child group, the single loop condition is enough to
            // establish all four child bounds. This is the common path for a
            // queue-sized heap and avoids three bounds branches per level.
            while new_length >= HEAP_ARITY + 1
                && parent <= (new_length - (HEAP_ARITY + 1)) >> HEAP_ARITY_SHIFT
            {
                let first_child = (parent << HEAP_ARITY_SHIFT) + 1;
                let mut child = first_child;
                let mut child_entry = unsafe { self.entry(child) };

                let candidate = unsafe { self.entry(first_child + 1) };
                if comes_before(candidate, child_entry) {
                    child = first_child + 1;
                    child_entry = candidate;
                }
                let candidate = unsafe { self.entry(first_child + 2) };
                if comes_before(candidate, child_entry) {
                    child = first_child + 2;
                    child_entry = candidate;
                }
                let candidate = unsafe { self.entry(first_child + 3) };
                if comes_before(candidate, child_entry) {
                    child = first_child + 3;
                    child_entry = candidate;
                }
                if !comes_before(child_entry, last) {
                    unsafe { self.write_entry(parent, last) };
                    return Some(root);
                }
                unsafe { self.write_entry(parent, child_entry) };
                parent = child;
            }

            // Only the final partial child group needs per-child bounds
            // checks. It has at most three comparisons and normally executes
            // once.
            let first_child = (parent << HEAP_ARITY_SHIFT) + 1;
            if first_child < new_length {
                let mut child = first_child;
                let mut child_entry = unsafe { self.entry(child) };
                let mut next_child = first_child + 1;
                while next_child < new_length {
                    let candidate = unsafe { self.entry(next_child) };
                    if comes_before(candidate, child_entry) {
                        child = next_child;
                        child_entry = candidate;
                    }
                    next_child += 1;
                }
                if comes_before(child_entry, last) {
                    unsafe { self.write_entry(parent, child_entry) };
                    parent = child;
                }
            }
            unsafe { self.write_entry(parent, last) };
        }
        Some(root)
    }
}

#[inline]
fn comes_before(left: PublishedEntry, right: PublishedEntry) -> bool {
    left.priority < right.priority
        || (left.priority == right.priority && left.slot_index < right.slot_index)
}

struct ReservationState {
    free_slots: FreeSlotStack,
}

struct FreeSlotStack {
    slots: Vec<usize>,
    length: usize,
}

impl FreeSlotStack {
    #[inline]
    fn pop(&mut self) -> Option<usize> {
        if self.length == 0 {
            return None;
        }
        self.length -= 1;
        Some(unsafe { *self.slots.get_unchecked(self.length) })
    }

    #[inline]
    fn push(&mut self, slot_index: usize) {
        debug_assert!(self.length < self.slots.len());
        unsafe { *self.slots.get_unchecked_mut(self.length) = slot_index };
        self.length += 1;
    }
}

struct PublishedState {
    heap: PublishedEntryHeap,
}

struct QueueState {
    max_value_size: usize,
    slots: Vec<UnsafeCell<Slot>>,
    reservation: SpinMutex<ReservationState>,
    published: SpinMutex<PublishedState>,
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
        let free_slots = FreeSlotStack {
            slots: free_slots,
            length: capacity,
        };

        Ok(Self {
            producer_count,
            consumer_count,
            state: Arc::new(QueueState {
                max_value_size,
                slots,
                reservation: SpinMutex::new(ReservationState { free_slots }),
                published: SpinMutex::new(PublishedState {
                    heap: PublishedEntryHeap::new(capacity)?,
                }),
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
            let mut published = self.state.published.lock();

            // Publication is short-held and only touches heap/index metadata.
            // The payload was fully copied before this lock was acquired.
            published.heap.push(PublishedEntry {
                priority,
                slot_index,
            });
        }
        STATUS_OK
    }
}

pub struct Consumer {
    state: Arc<QueueState>,
}

impl Consumer {
    fn try_dequeue(&self, output: &mut [u8]) -> Result<usize, u32> {
        let (slot_index, length) = {
            let mut published = self.state.published.lock();
            let Some(entry) = published.heap.peek() else {
                return Err(STATUS_EMPTY);
            };
            let length = unsafe { self.state.slot(entry.slot_index) }.len();
            if length > output.len() {
                return Err(STATUS_INVALID);
            }
            // Removing the index transfers exclusive slot ownership to this
            // consumer. Do not hold publication while copying the payload.
            let entry = published
                .heap
                .pop()
                .expect("heap peek implies a heap item");
            (entry.slot_index, length)
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

    #[test]
    fn four_way_heap_preserves_entry_order_and_capacity() {
        let mut heap = PublishedEntryHeap::new(16).unwrap();
        let initial_capacity = heap.entries.capacity();
        let entries = [
            PublishedEntry {
                priority: 9,
                slot_index: 14,
            },
            PublishedEntry {
                priority: 1,
                slot_index: 11,
            },
            PublishedEntry {
                priority: 4,
                slot_index: 3,
            },
            PublishedEntry {
                priority: 1,
                slot_index: 2,
            },
            PublishedEntry {
                priority: 7,
                slot_index: 8,
            },
            PublishedEntry {
                priority: 1,
                slot_index: 7,
            },
            PublishedEntry {
                priority: 4,
                slot_index: 1,
            },
            PublishedEntry {
                priority: 0,
                slot_index: 15,
            },
            PublishedEntry {
                priority: 12,
                slot_index: 0,
            },
            PublishedEntry {
                priority: 4,
                slot_index: 12,
            },
            PublishedEntry {
                priority: 2,
                slot_index: 6,
            },
            PublishedEntry {
                priority: 2,
                slot_index: 5,
            },
            PublishedEntry {
                priority: 8,
                slot_index: 10,
            },
            PublishedEntry {
                priority: 3,
                slot_index: 4,
            },
            PublishedEntry {
                priority: 6,
                slot_index: 9,
            },
            PublishedEntry {
                priority: 5,
                slot_index: 13,
            },
        ];
        for entry in entries {
            heap.push(entry);
            assert_eq!(heap.entries.capacity(), initial_capacity);
        }

        for expected in [
            PublishedEntry {
                priority: 0,
                slot_index: 15,
            },
            PublishedEntry {
                priority: 1,
                slot_index: 2,
            },
            PublishedEntry {
                priority: 1,
                slot_index: 7,
            },
            PublishedEntry {
                priority: 1,
                slot_index: 11,
            },
            PublishedEntry {
                priority: 2,
                slot_index: 5,
            },
            PublishedEntry {
                priority: 2,
                slot_index: 6,
            },
            PublishedEntry {
                priority: 3,
                slot_index: 4,
            },
            PublishedEntry {
                priority: 4,
                slot_index: 1,
            },
            PublishedEntry {
                priority: 4,
                slot_index: 3,
            },
            PublishedEntry {
                priority: 4,
                slot_index: 12,
            },
            PublishedEntry {
                priority: 5,
                slot_index: 13,
            },
            PublishedEntry {
                priority: 6,
                slot_index: 9,
            },
            PublishedEntry {
                priority: 7,
                slot_index: 8,
            },
            PublishedEntry {
                priority: 8,
                slot_index: 10,
            },
            PublishedEntry {
                priority: 9,
                slot_index: 14,
            },
            PublishedEntry {
                priority: 12,
                slot_index: 0,
            },
        ] {
            let actual = heap.pop().expect("entry conservation");
            assert_eq!(actual.priority, expected.priority);
            assert_eq!(actual.slot_index, expected.slot_index);
            assert_eq!(heap.entries.capacity(), initial_capacity);
        }
        assert!(heap.pop().is_none());
    }

    #[test]
    fn four_way_heap_handles_all_child_group_shapes() {
        for capacity in 1..=128 {
            let mut heap = PublishedEntryHeap::new(capacity).unwrap();
            let mut expected = Vec::with_capacity(capacity);
            let mut state = capacity as u64;
            for slot_index in 0..capacity {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let entry = PublishedEntry {
                    priority: state ^ (state >> 29),
                    slot_index,
                };
                heap.push(entry);
                expected.push(entry);
            }
            expected.sort_by(|left, right| {
                if comes_before(*left, *right) {
                    std::cmp::Ordering::Less
                } else if comes_before(*right, *left) {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            });

            for expected_entry in expected {
                assert_eq!(heap.pop(), Some(expected_entry));
            }
            assert!(heap.pop().is_none());
        }
    }
}
