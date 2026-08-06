#![deny(unsafe_op_in_unsafe_fn)]

mod ffi;

use std::cell::UnsafeCell;
use std::sync::{Arc, Mutex};

const ABI_VERSION: u32 = 1;
const STATUS_OK: u32 = 0;
const STATUS_FULL: u32 = 1;
const STATUS_EMPTY: u32 = 2;
const STATUS_INVALID: u32 = 3;
const STATUS_INTERNAL_ERROR: u32 = 4;

struct Slot {
    priority: u64,
    sequence: u64,
    value: Vec<u8>,
}

impl Slot {
    fn new() -> Self {
        Self {
            priority: 0,
            sequence: 0,
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

struct SlotIndexHeap {
    indices: Vec<usize>,
}

impl SlotIndexHeap {
    fn new(capacity: usize) -> Result<Self, u32> {
        let mut indices = Vec::new();
        indices
            .try_reserve_exact(capacity)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;
        Ok(Self { indices })
    }

    fn peek(&self) -> Option<usize> {
        self.indices.first().copied()
    }

    unsafe fn push(&mut self, index: usize, slots: &[UnsafeCell<Slot>]) {
        self.indices.push(index);
        let mut child = self.indices.len() - 1;
        while child != 0 {
            let parent = (child - 1) / 2;
            if !unsafe { comes_before(self.indices[child], self.indices[parent], slots) } {
                break;
            }
            self.indices.swap(child, parent);
            child = parent;
        }
    }

    unsafe fn pop(&mut self, slots: &[UnsafeCell<Slot>]) -> Option<usize> {
        let root = self.indices.first().copied()?;
        let last = self.indices.pop().expect("heap root implies a last item");
        if !self.indices.is_empty() {
            self.indices[0] = last;
            let mut parent = 0;
            loop {
                let left = parent * 2 + 1;
                if left >= self.indices.len() {
                    break;
                }
                let right = left + 1;
                let mut child = left;
                if right < self.indices.len()
                    && unsafe { comes_before(self.indices[right], self.indices[left], slots) }
                {
                    child = right;
                }
                if !unsafe { comes_before(self.indices[child], self.indices[parent], slots) } {
                    break;
                }
                self.indices.swap(parent, child);
                parent = child;
            }
        }
        Some(root)
    }
}

unsafe fn comes_before(left: usize, right: usize, slots: &[UnsafeCell<Slot>]) -> bool {
    let left = unsafe { &*slots[left].get() };
    let right = unsafe { &*slots[right].get() };
    left.priority < right.priority
        || (left.priority == right.priority && left.sequence < right.sequence)
}

struct ReservationState {
    next_sequence: u64,
    free_slots: Vec<usize>,
}

struct PublishedState {
    heap: SlotIndexHeap,
}

struct QueueState {
    max_value_size: usize,
    slots: Vec<UnsafeCell<Slot>>,
    reservation: Mutex<ReservationState>,
    published: Mutex<PublishedState>,
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
                reservation: Mutex::new(ReservationState {
                    next_sequence: 0,
                    free_slots,
                }),
                published: Mutex::new(PublishedState {
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
        let (slot_index, sequence) = {
            let Ok(mut reservation) = self.state.reservation.lock() else {
                return STATUS_INTERNAL_ERROR;
            };
            let Some(slot_index) = reservation.free_slots.pop() else {
                return STATUS_FULL;
            };
            let sequence = reservation.next_sequence;
            reservation.next_sequence = reservation.next_sequence.wrapping_add(1);
            (slot_index, sequence)
        };

        // The reservation gives this producer exclusive access to the slot.
        // Keep the borrowed input copy independent from both shared queue locks.
        if unsafe { self.state.slot_mut(slot_index) }
            .copy_value(data)
            .is_err()
        {
            let Ok(mut reservation) = self.state.reservation.lock() else {
                return STATUS_INTERNAL_ERROR;
            };
            reservation.free_slots.push(slot_index);
            return STATUS_INTERNAL_ERROR;
        }
        {
            let slot = unsafe { self.state.slot_mut(slot_index) };
            slot.priority = priority;
            slot.sequence = sequence;
        }

        let Ok(mut published) = self.state.published.lock() else {
            let Ok(mut reservation) = self.state.reservation.lock() else {
                return STATUS_INTERNAL_ERROR;
            };
            reservation.free_slots.push(slot_index);
            return STATUS_INTERNAL_ERROR;
        };

        // Publication is short-held and only touches heap/index metadata. The
        // payload was fully copied before this lock was acquired.
        unsafe { published.heap.push(slot_index, &self.state.slots) };
        STATUS_OK
    }
}

pub struct Consumer {
    state: Arc<QueueState>,
}

impl Consumer {
    fn try_dequeue(&self, output: &mut [u8]) -> Result<usize, u32> {
        let (slot_index, length) = {
            let Ok(mut published) = self.state.published.lock() else {
                return Err(STATUS_INTERNAL_ERROR);
            };
            let Some(slot_index) = published.heap.peek() else {
                return Err(STATUS_EMPTY);
            };
            let length = unsafe { self.state.slot(slot_index) }.value.len();
            if length > output.len() {
                return Err(STATUS_INVALID);
            }
            // Removing the index transfers exclusive slot ownership to this
            // consumer. Do not hold publication while copying the payload.
            let slot_index = unsafe { published.heap.pop(&self.state.slots) }
                .expect("heap peek implies a heap item");
            (slot_index, length)
        };

        {
            let slot = unsafe { self.state.slot(slot_index) };
            output[..length].copy_from_slice(&slot.value);
        }

        // The consumer still owns the slot while clearing it. Only then make
        // it visible to a producer again.
        unsafe { self.state.slot_mut(slot_index) }.value.clear();
        let Ok(mut reservation) = self.state.reservation.lock() else {
            return Err(STATUS_INTERNAL_ERROR);
        };
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
