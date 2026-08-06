#![deny(unsafe_op_in_unsafe_fn)]

mod ffi;

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

    fn push(&mut self, index: usize, slots: &[Slot]) {
        self.indices.push(index);
        let mut child = self.indices.len() - 1;
        while child != 0 {
            let parent = (child - 1) / 2;
            if !comes_before(self.indices[child], self.indices[parent], slots) {
                break;
            }
            self.indices.swap(child, parent);
            child = parent;
        }
    }

    fn pop(&mut self, slots: &[Slot]) -> Option<usize> {
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
                    && comes_before(self.indices[right], self.indices[left], slots)
                {
                    child = right;
                }
                if !comes_before(self.indices[child], self.indices[parent], slots) {
                    break;
                }
                self.indices.swap(parent, child);
                parent = child;
            }
        }
        Some(root)
    }
}

fn comes_before(left: usize, right: usize, slots: &[Slot]) -> bool {
    let left = &slots[left];
    let right = &slots[right];
    left.priority < right.priority
        || (left.priority == right.priority && left.sequence < right.sequence)
}

struct Values {
    next_sequence: u64,
    slots: Vec<Slot>,
    free_slots: Vec<usize>,
    heap: SlotIndexHeap,
}

impl Values {
    fn publish(&mut self, slot_index: usize) {
        self.heap.push(slot_index, &self.slots);
    }

    fn remove_min(&mut self) -> Option<usize> {
        self.heap.pop(&self.slots)
    }
}

struct QueueState {
    max_value_size: usize,
    values: Mutex<Values>,
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
            slots.push(Slot::new());
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
                values: Mutex::new(Values {
                    next_sequence: 0,
                    slots,
                    free_slots,
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
        let Ok(mut values) = self.state.values.lock() else {
            return STATUS_INTERNAL_ERROR;
        };
        let Some(slot_index) = values.free_slots.pop() else {
            return STATUS_FULL;
        };

        let sequence = values.next_sequence;
        let slot = &mut values.slots[slot_index];
        if slot.copy_value(data).is_err() {
            values.free_slots.push(slot_index);
            return STATUS_INTERNAL_ERROR;
        }
        slot.priority = priority;
        slot.sequence = sequence;
        values.next_sequence = values.next_sequence.wrapping_add(1);
        values.publish(slot_index);
        STATUS_OK
    }
}

pub struct Consumer {
    state: Arc<QueueState>,
}

impl Consumer {
    fn try_dequeue(&self, output: &mut [u8]) -> Result<usize, u32> {
        let Ok(mut values) = self.state.values.lock() else {
            return Err(STATUS_INTERNAL_ERROR);
        };
        let Some(slot_index) = values.heap.peek() else {
            return Err(STATUS_EMPTY);
        };
        let slot = &values.slots[slot_index];
        if slot.value.len() > output.len() {
            return Err(STATUS_INVALID);
        }
        let length = slot.value.len();
        output[..length].copy_from_slice(&slot.value);
        let slot_index = values
            .remove_min()
            .expect("heap peek implies a heap item");
        values.slots[slot_index].value.clear();
        values.free_slots.push(slot_index);
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
