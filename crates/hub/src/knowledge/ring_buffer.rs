//! Bounded ring buffer over `VecDeque`. Used for in-memory recent-pass
//! history and recent-insert error tracking. Single canonical
//! implementation reused across the knowledge subsystem.

use std::collections::VecDeque;

/// Bounded FIFO. `push` evicts the oldest entry once the capacity is
/// reached. Capacity is fixed at construction; `capacity == 0` is
/// rejected so iteration always returns at most `capacity` items.
#[derive(Debug, Clone)]
pub struct RingBuffer<T> {
    inner: VecDeque<T>,
    capacity: usize,
}

impl<T> RingBuffer<T> {
    /// # Panics
    /// Panics if `capacity == 0` — a zero-cap buffer would silently
    /// drop every push, which is never the intent.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "RingBuffer capacity must be > 0");
        Self {
            inner: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, item: T) {
        if self.inner.len() == self.capacity {
            self.inner.pop_front();
        }
        self.inner.push_back(item);
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.inner.iter()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_below_capacity_keeps_all_entries() {
        let mut rb = RingBuffer::new(3);
        rb.push(1);
        rb.push(2);
        assert_eq!(rb.len(), 2);
        let v: Vec<i32> = rb.iter().copied().collect();
        assert_eq!(v, vec![1, 2]);
    }

    #[test]
    fn push_at_capacity_evicts_oldest() {
        let mut rb = RingBuffer::new(2);
        rb.push(1);
        rb.push(2);
        rb.push(3);
        let v: Vec<i32> = rb.iter().copied().collect();
        assert_eq!(v, vec![2, 3]);
        assert_eq!(rb.len(), 2);
    }

    #[test]
    fn iter_returns_oldest_first() {
        let mut rb = RingBuffer::new(4);
        for i in 0..4 {
            rb.push(i);
        }
        let v: Vec<i32> = rb.iter().copied().collect();
        assert_eq!(v, vec![0, 1, 2, 3]);
    }

    #[test]
    fn rev_iter_returns_newest_first() {
        let mut rb = RingBuffer::new(4);
        for i in 0..4 {
            rb.push(i);
        }
        let v: Vec<i32> = rb.iter().rev().copied().collect();
        assert_eq!(v, vec![3, 2, 1, 0]);
    }

    #[test]
    #[should_panic(expected = "capacity must be > 0")]
    fn zero_capacity_panics() {
        let _: RingBuffer<i32> = RingBuffer::new(0);
    }

    #[test]
    fn many_pushes_past_capacity_keep_window() {
        let mut rb = RingBuffer::new(3);
        for i in 0..100 {
            rb.push(i);
        }
        let v: Vec<i32> = rb.iter().copied().collect();
        assert_eq!(v, vec![97, 98, 99]);
    }
}
