//! Lock-free SPSC ring of stereo `f32` frames.
//!
//! Capacity is a power of two. Storage is a pair of `AtomicU32` arrays (f32 bit
//! patterns, relaxed access) so the implementation needs no `unsafe`; ordering
//! is established by the Release/Acquire pair on the head/tail indices.
//!
//! **Overrun policy:** the producer never overwrites unread data. If the ring is
//! full, [`Producer::push_frames`] stores what fits, drops the *excess new
//! frames* and counts them ([`Producer::overruns`] = events,
//! [`Producer::dropped_frames`] = frames). Latency is trimmed by the consumer
//! side with [`Consumer::discard`] (drops the *oldest* frames), which the drift
//! controller uses when the fill grows beyond twice the target.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

#[repr(align(64))]
struct Padded(AtomicUsize);

struct Shared {
    l: Box<[AtomicU32]>,
    r: Box<[AtomicU32]>,
    mask: usize,
    /// Next frame to read (written by the consumer only). Monotonic, wrapping.
    head: Padded,
    /// Next frame to write (written by the producer only). Monotonic, wrapping.
    tail: Padded,
    overrun_events: AtomicU64,
    dropped: AtomicU64,
}

impl Shared {
    fn fill(&self) -> usize {
        let t = self.tail.0.load(Ordering::Acquire);
        let h = self.head.0.load(Ordering::Acquire);
        t.wrapping_sub(h).min(self.mask + 1)
    }
}

/// Creates a ring holding at least `min_capacity` frames (rounded up to a power of two, minimum 2).
pub fn channel(min_capacity: usize) -> (Producer, Consumer) {
    let cap = min_capacity.max(2).next_power_of_two();
    let mk = || (0..cap).map(|_| AtomicU32::new(0)).collect::<Vec<_>>().into_boxed_slice();
    let sh = Arc::new(Shared {
        l: mk(),
        r: mk(),
        mask: cap - 1,
        head: Padded(AtomicUsize::new(0)),
        tail: Padded(AtomicUsize::new(0)),
        overrun_events: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
    });
    (Producer { sh: sh.clone() }, Consumer { sh })
}

/// Writing end (capture thread).
pub struct Producer {
    sh: Arc<Shared>,
}

/// Reading end (audio callback).
pub struct Consumer {
    sh: Arc<Shared>,
}

impl Producer {
    /// Pushes `min(l.len(), r.len())` frames; returns how many were stored. The
    /// rest is dropped and counted as an overrun. Wait-free, allocation-free.
    pub fn push_frames(&mut self, l: &[f32], r: &[f32]) -> usize {
        let sh = &*self.sh;
        let n = l.len().min(r.len());
        let tail = sh.tail.0.load(Ordering::Relaxed);
        let head = sh.head.0.load(Ordering::Acquire);
        let free = (sh.mask + 1) - tail.wrapping_sub(head).min(sh.mask + 1);
        let k = n.min(free);
        for i in 0..k {
            let idx = tail.wrapping_add(i) & sh.mask;
            sh.l[idx].store(l[i].to_bits(), Ordering::Relaxed);
            sh.r[idx].store(r[i].to_bits(), Ordering::Relaxed);
        }
        sh.tail.0.store(tail.wrapping_add(k), Ordering::Release);
        if k < n {
            sh.overrun_events.fetch_add(1, Ordering::Relaxed);
            sh.dropped.fetch_add((n - k) as u64, Ordering::Relaxed);
        }
        k
    }

    /// Frames currently stored.
    pub fn fill(&self) -> usize {
        self.sh.fill()
    }
    /// Total capacity in frames (power of two).
    pub fn capacity(&self) -> usize {
        self.sh.mask + 1
    }
    /// Number of push calls that had to drop frames.
    pub fn overruns(&self) -> u64 {
        self.sh.overrun_events.load(Ordering::Relaxed)
    }
    /// Total frames dropped by the producer.
    pub fn dropped_frames(&self) -> u64 {
        self.sh.dropped.load(Ordering::Relaxed)
    }
}

impl Consumer {
    /// Pops up to `min(l.len(), r.len())` frames; returns how many were popped.
    pub fn pop_frames(&mut self, l: &mut [f32], r: &mut [f32]) -> usize {
        let sh = &*self.sh;
        let head = sh.head.0.load(Ordering::Relaxed);
        let tail = sh.tail.0.load(Ordering::Acquire);
        let avail = tail.wrapping_sub(head).min(sh.mask + 1);
        let k = l.len().min(r.len()).min(avail);
        for i in 0..k {
            let idx = head.wrapping_add(i) & sh.mask;
            l[i] = f32::from_bits(sh.l[idx].load(Ordering::Relaxed));
            r[i] = f32::from_bits(sh.r[idx].load(Ordering::Relaxed));
        }
        sh.head.0.store(head.wrapping_add(k), Ordering::Release);
        k
    }

    /// Drops the `n` oldest frames (or all if fewer); returns how many were dropped.
    pub fn discard(&mut self, n: usize) -> usize {
        let sh = &*self.sh;
        let head = sh.head.0.load(Ordering::Relaxed);
        let tail = sh.tail.0.load(Ordering::Acquire);
        let k = n.min(tail.wrapping_sub(head).min(sh.mask + 1));
        sh.head.0.store(head.wrapping_add(k), Ordering::Release);
        k
    }

    /// Frames currently stored.
    pub fn fill(&self) -> usize {
        self.sh.fill()
    }
    /// Total capacity in frames (power of two).
    pub fn capacity(&self) -> usize {
        self.sh.mask + 1
    }
    /// Number of producer pushes that dropped frames.
    pub fn overruns(&self) -> u64 {
        self.sh.overrun_events.load(Ordering::Relaxed)
    }
    /// Total frames dropped by the producer.
    pub fn dropped_frames(&self) -> u64 {
        self.sh.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_rounds_up() {
        let (p, c) = channel(1000);
        assert_eq!(p.capacity(), 1024);
        assert_eq!(c.capacity(), 1024);
        assert_eq!(channel(0).0.capacity(), 2);
    }

    #[test]
    fn push_pop_fifo_with_wrap() {
        let (mut p, mut c) = channel(8);
        let mut next = 0.0f32;
        let mut expect = 0.0f32;
        for _ in 0..100 {
            let l: Vec<f32> = (0..5).map(|i| next + i as f32).collect();
            let r: Vec<f32> = l.iter().map(|x| -x).collect();
            assert_eq!(p.push_frames(&l, &r), 5);
            next += 5.0;
            let (mut ol, mut or) = ([0.0; 5], [0.0; 5]);
            assert_eq!(c.pop_frames(&mut ol, &mut or), 5);
            for i in 0..5 {
                assert_eq!(ol[i], expect);
                assert_eq!(or[i], -expect);
                expect += 1.0;
            }
        }
        assert_eq!(c.fill(), 0);
    }

    #[test]
    fn overrun_drops_new_frames_and_counts() {
        let (mut p, mut c) = channel(8);
        let v: Vec<f32> = (0..12).map(|i| i as f32).collect();
        assert_eq!(p.push_frames(&v, &v), 8);
        assert_eq!(p.overruns(), 1);
        assert_eq!(p.dropped_frames(), 4);
        assert_eq!(c.overruns(), 1);
        assert_eq!(p.fill(), 8);
        let (mut l, mut r) = ([0.0; 8], [0.0; 8]);
        assert_eq!(c.pop_frames(&mut l, &mut r), 8);
        assert_eq!(l, [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        assert_eq!(p.push_frames(&v[..3], &v[..3]), 3);
        assert_eq!(p.overruns(), 1);
    }

    #[test]
    fn underflow_and_discard() {
        let (mut p, mut c) = channel(16);
        let v: Vec<f32> = (0..10).map(|i| i as f32).collect();
        p.push_frames(&v, &v);
        assert_eq!(c.discard(4), 4);
        assert_eq!(c.fill(), 6);
        let (mut l, mut r) = ([0.0; 10], [0.0; 10]);
        assert_eq!(c.pop_frames(&mut l, &mut r), 6);
        assert_eq!(l[0], 4.0);
        assert_eq!(c.pop_frames(&mut l, &mut r), 0);
        assert_eq!(c.discard(5), 0);
    }

    #[test]
    fn mismatched_lengths_use_min() {
        let (mut p, c) = channel(8);
        assert_eq!(p.push_frames(&[1.0, 2.0, 3.0], &[1.0, 2.0]), 2);
        assert_eq!(c.fill(), 2);
    }

    #[test]
    fn threaded_integrity() {
        let (mut p, mut c) = channel(256);
        const N: usize = 400_000;
        let prod = std::thread::spawn(move || {
            let mut i = 0usize;
            while i < N {
                let chunk = 37.min(N - i);
                let l: Vec<f32> = (i..i + chunk).map(|x| x as f32).collect();
                let r: Vec<f32> = (i..i + chunk).map(|x| (x as f32) * 2.0).collect();
                let mut off = 0;
                while off < chunk {
                    let k = p.push_frames(&l[off..], &r[off..]);
                    off += k;
                    if k == 0 {
                        std::thread::yield_now();
                    }
                }
                i += chunk;
            }
        });
        let (mut l, mut r) = ([0.0f32; 64], [0.0f32; 64]);
        let mut seen = 0usize;
        while seen < N {
            let k = c.pop_frames(&mut l, &mut r);
            for j in 0..k {
                assert_eq!(l[j], (seen + j) as f32);
                assert_eq!(r[j], ((seen + j) as f32) * 2.0);
            }
            seen += k;
            if k == 0 {
                std::thread::yield_now();
            }
        }
        prod.join().unwrap();
    }
}
