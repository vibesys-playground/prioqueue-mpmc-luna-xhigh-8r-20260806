#![deny(unsafe_op_in_unsafe_fn)]

mod ffi;

use std::cell::UnsafeCell;
use std::hint::spin_loop;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const ABI_VERSION: u32 = 1;
const STATUS_OK: u32 = 0;
const STATUS_FULL: u32 = 1;
const STATUS_EMPTY: u32 = 2;
const STATUS_INVALID: u32 = 3;
const STATUS_INTERNAL_ERROR: u32 = 4;

struct Slot {
    value: Vec<u8>,
}

impl Slot {
    fn new() -> Self {
        Self {
            value: Vec::new(),
        }
    }

    fn copy_value(&mut self, data: &[u8]) -> Result<(), ()> {
        self.value.clear();
        if self.value.capacity() < data.len() {
            self.value.try_reserve_exact(data.len()).map_err(|_| ())?;
        }
        self.value.extend_from_slice(data);
        Ok(())
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

struct SlotIndexHeap {
    entries: Vec<PublishedEntry>,
}

#[derive(Clone, Copy)]
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

struct ReservationState {
    free_slots: Vec<usize>,
}

struct PublishedState {
    heap: SlotIndexHeap,
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
            slots.push(UnsafeCell::new(Slot::new()));
        }

        let mut free_slots = Vec::new();
        free_slots
            .try_reserve_exact(capacity)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;
        for index in (0..capacity).rev() {
            free_slots.push(index);
        }

        Ok(Self {
            producer_count,
            consumer_count,
            state: Arc::new(QueueState {
                max_value_size,
                slots,
                reservation: SpinMutex::new(ReservationState { free_slots }),
                published: SpinMutex::new(PublishedState {
                    heap: SlotIndexHeap::new(capacity)?,
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
            let length = unsafe { self.state.slot(entry.slot_index) }.value.len();
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
            output[..length].copy_from_slice(&slot.value);
        }

        // The consumer still owns the slot while clearing it. Only then make
        // it visible to a producer again.
        unsafe { self.state.slot_mut(slot_index) }.value.clear();
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
}
