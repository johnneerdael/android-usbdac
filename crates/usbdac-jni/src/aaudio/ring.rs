//! A single-producer, single-consumer byte ring for the AAudio data callback.
//!
//! The host's writer thread is the producer; AAudio's real-time callback thread is the consumer.
//! The consumer never takes a lock and never allocates: it reads two atomics, copies, and stores
//! one. The producer is serialised by the caller (`Playback` holds its mutex around every write).
//!
//! Indices are monotonic byte counts; the slot of index `i` is `i % capacity`. The producer
//! overwrites a slot only when the consumer has moved past it, and the consumer reads only what
//! the producer has published, so the two never touch the same byte at once.
//!
//! A flush is a request, not a reset: the producer records the write index in `discard`, and the
//! consumer, at the start of its next callback, moves its read index there. Until then the
//! producer cannot reuse the dropped bytes (the callback may be reading them), which costs at
//! most one callback period of room after a flush.

use std::cell::{Cell, UnsafeCell};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub(super) struct SpscRing {
    slots: Box<[UnsafeCell<u8>]>,
    /// Total bytes the consumer has taken (or skipped).
    head: AtomicU64,
    /// Total bytes the producer has published.
    tail: AtomicU64,
    /// Read index the consumer should skip ahead to at its next opportunity.
    discard: AtomicU64,
}

// SAFETY: the slots are only written by the producer in the region the consumer has released, and
// only read by the consumer in the region the producer has published; the atomics order those
// accesses (release on publish and on release, acquire on the matching loads).
unsafe impl Send for SpscRing {}
unsafe impl Sync for SpscRing {}

impl SpscRing {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "a ring needs room");
        Self {
            slots: (0..capacity).map(|_| UnsafeCell::new(0)).collect(),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            discard: AtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Bytes the consumer would read now (what a flush has not yet dropped is included).
    pub fn len(&self) -> usize {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Acquire);
        tail.saturating_sub(head) as usize
    }

    /// Bytes the consumer would read once it has applied a pending flush.
    pub fn len_after_flush(&self) -> usize {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self
            .head
            .load(Ordering::Acquire)
            .max(self.discard.load(Ordering::Acquire));
        tail.saturating_sub(head) as usize
    }

    /// Room for the producer now.
    pub fn free(&self) -> usize {
        self.capacity() - self.len()
    }

    /// Producer: copies as much of `data` as fits and returns the count.
    fn write(&self, data: &[u8]) -> usize {
        let capacity = self.capacity();
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let used = (tail - head) as usize;
        let count = data.len().min(capacity - used);
        if count == 0 {
            return 0;
        }
        let start = (tail % capacity as u64) as usize;
        let first = count.min(capacity - start);
        // SAFETY: [tail, tail + count) is free (the consumer is past it), and `UnsafeCell<u8>`
        // has the layout of `u8`, so the slots form a contiguous byte buffer.
        unsafe {
            let base = self.slots.as_ptr() as *mut u8;
            std::ptr::copy_nonoverlapping(data.as_ptr(), base.add(start), first);
            std::ptr::copy_nonoverlapping(data.as_ptr().add(first), base, count - first);
        }
        self.tail.store(tail + count as u64, Ordering::Release);
        count
    }

    /// Producer: asks the consumer to drop everything published so far.
    fn request_discard_all(&self) {
        self.discard
            .store(self.tail.load(Ordering::Acquire), Ordering::Release);
    }

    /// Consumer: carries out a pending flush. Returns whether anything was skipped.
    fn apply_discard(&self) -> bool {
        let target = self.discard.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Relaxed);
        if target > head {
            self.head.store(target, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// Consumer: copies up to `out.len()` bytes and returns the count.
    fn read(&self, out: &mut [u8]) -> usize {
        let capacity = self.capacity();
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let count = out.len().min((tail - head) as usize);
        if count == 0 {
            return 0;
        }
        let start = (head % capacity as u64) as usize;
        let first = count.min(capacity - start);
        // SAFETY: [head, head + count) was published by the producer and is not rewritten until
        // `head` moves past it.
        unsafe {
            let base = self.slots.as_ptr() as *const u8;
            std::ptr::copy_nonoverlapping(base.add(start), out.as_mut_ptr(), first);
            std::ptr::copy_nonoverlapping(base, out.as_mut_ptr().add(first), count - first);
        }
        self.head.store(head + count as u64, Ordering::Release);
        count
    }
}

/// Unique producer and consumer handles. Neither endpoint is Sync, and mutating
/// the ring requires a mutable endpoint, preventing safe concurrent reuse.
pub(super) struct Producer {
    ring: Arc<SpscRing>,
    _not_sync: PhantomData<Cell<()>>,
}
pub(super) struct Consumer {
    ring: Arc<SpscRing>,
    _not_sync: PhantomData<Cell<()>>,
}

pub(super) fn split(capacity: usize) -> (Arc<SpscRing>, Producer, Consumer) {
    let ring = Arc::new(SpscRing::new(capacity));
    (
        ring.clone(),
        Producer {
            ring: ring.clone(),
            _not_sync: PhantomData,
        },
        Consumer {
            ring,
            _not_sync: PhantomData,
        },
    )
}
impl Producer {
    pub(super) fn write(&mut self, data: &[u8]) -> usize {
        self.ring.write(data)
    }
    pub(super) fn request_discard_all(&mut self) {
        self.ring.request_discard_all()
    }
}
impl Consumer {
    pub(super) fn read(&mut self, out: &mut [u8]) -> usize {
        self.ring.read(out)
    }
    pub(super) fn apply_discard(&mut self) -> bool {
        self.ring.apply_discard()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_come_out_in_order() {
        let (ring, mut producer, mut consumer) = split(8);
        assert_eq!(producer.write(&[1, 2, 3, 4, 5]), 5);
        let mut out = [0u8; 3];
        assert_eq!(consumer.read(&mut out), 3);
        assert_eq!(out, [1, 2, 3]);
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.free(), 6);
    }

    #[test]
    fn a_full_ring_takes_nothing_more() {
        let (ring, mut producer, mut consumer) = split(4);
        assert_eq!(producer.write(&[1, 2, 3, 4, 5, 6]), 4);
        assert_eq!(producer.write(&[9]), 0);
        assert_eq!(ring.free(), 0);
        let mut out = [0u8; 8];
        assert_eq!(consumer.read(&mut out), 4);
        assert_eq!(&out[..4], &[1, 2, 3, 4]);
        assert_eq!(consumer.read(&mut out), 0);
    }

    #[test]
    fn the_ring_wraps_around() {
        let (_ring, mut producer, mut consumer) = split(5);
        let mut out = [0u8; 5];
        for round in 0..20u8 {
            let data = [round, round + 1, round + 2, round + 3];
            assert_eq!(producer.write(&data), 4);
            assert_eq!(consumer.read(&mut out[..4]), 4);
            assert_eq!(&out[..4], &data);
        }
    }

    #[test]
    fn a_discard_is_applied_by_the_consumer_only() {
        let (ring, mut producer, mut consumer) = split(16);
        producer.write(&[1, 2, 3, 4, 5, 6]);
        producer.request_discard_all();
        // The bytes still occupy the ring until the consumer lets go of them.
        assert_eq!(ring.len(), 6);
        assert_eq!(ring.len_after_flush(), 0);
        assert_eq!(ring.free(), 10);
        assert!(consumer.apply_discard());
        assert_eq!(ring.len(), 0);
        assert!(!consumer.apply_discard());
        producer.write(&[7, 8]);
        let mut out = [0u8; 8];
        assert_eq!(consumer.read(&mut out), 2);
        assert_eq!(&out[..2], &[7, 8]);
    }

    #[test]
    fn bytes_written_after_a_discard_request_survive_it() {
        let (ring, mut producer, mut consumer) = split(16);
        producer.write(&[1, 2, 3]);
        producer.request_discard_all();
        producer.write(&[4, 5]);
        assert_eq!(ring.len_after_flush(), 2);
        consumer.apply_discard();
        let mut out = [0u8; 8];
        assert_eq!(consumer.read(&mut out), 2);
        assert_eq!(&out[..2], &[4, 5]);
    }

    #[test]
    fn a_producer_and_a_consumer_on_two_threads_lose_and_reorder_nothing() {
        const TOTAL: usize = 2_000_000;
        let (ring, mut producer, mut consumer) = split(4099);
        let producer_thread = {
            std::thread::spawn(move || {
                let mut sent = 0usize;
                let mut chunk = [0u8; 700];
                while sent < TOTAL {
                    let n = chunk.len().min(TOTAL - sent);
                    for (i, byte) in chunk[..n].iter_mut().enumerate() {
                        *byte = ((sent + i) % 251) as u8;
                    }
                    let taken = producer.write(&chunk[..n]);
                    sent += taken;
                    if taken == 0 {
                        std::thread::yield_now();
                    }
                    // The bytes of a partial write are rebuilt from `sent` on the next round.
                }
            })
        };
        let mut received = 0usize;
        let mut out = [0u8; 513];
        while received < TOTAL {
            let n = consumer.read(&mut out);
            for (i, byte) in out[..n].iter().enumerate() {
                assert_eq!(*byte, ((received + i) % 251) as u8, "byte {}", received + i);
            }
            received += n;
            if n == 0 {
                std::thread::yield_now();
            }
        }
        producer_thread.join().unwrap();
        assert_eq!(ring.len(), 0);
    }
}
