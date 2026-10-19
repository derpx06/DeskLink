//! Bounded terminal output ring, addressed by monotonic byte offset.
//!
//! A reattaching client says "I last saw byte N"; if N is still in the ring we
//! replay from it byte-exactly and the client keeps its scrollback. If it has
//! aged out we fall back to a screen snapshot instead (see [`super::session`]).
//!
//! Offsets are absolute and monotonic for the life of the session, never
//! wrapping indices: an index into a ring that has since been pruned is a
//! silent correctness bug, an offset that is simply older than `tail` is a
//! question we can answer honestly.

use std::collections::VecDeque;

/// Small pushes are merged into the newest chunk up to this size, so the chunk
/// count stays near `capacity / CHUNK_TARGET` however the program writes. A
/// program printing one byte at a time would otherwise leave hundreds of
/// thousands of one-byte chunks for every replay to walk.
const CHUNK_TARGET: usize = 4 * 1024;

/// Retained output per session. Roughly two minutes of chatty build output —
/// enough that a screen-off pocket trip replays exactly, while a long absence
/// falls back to the snapshot rather than growing without bound.
pub(crate) const RING_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub(crate) struct OutputRing {
    chunks: VecDeque<Vec<u8>>,
    buffered: usize,
    capacity: usize,
    /// Absolute offset of the first byte still retained.
    tail: u64,
    /// Absolute offset one past the last byte produced.
    head: u64,
}

impl OutputRing {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            buffered: 0,
            capacity,
            tail: 0,
            head: 0,
        }
    }

    pub(crate) fn head(&self) -> u64 {
        self.head
    }

    #[cfg(test)]
    pub(crate) fn tail(&self) -> u64 {
        self.tail
    }

    /// Appends freshly read PTY bytes, pruning from the front to stay within
    /// `capacity`.
    ///
    /// A single write larger than the whole capacity keeps its tail rather than
    /// emptying the ring: dropping everything would strand `tail == head` with
    /// the bytes the client is about to ask for already gone.
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.head += bytes.len() as u64;
        match self.chunks.back_mut() {
            Some(last) if last.len() + bytes.len() <= CHUNK_TARGET => {
                last.extend_from_slice(bytes);
            }
            _ => self.chunks.push_back(bytes.to_vec()),
        }
        self.buffered += bytes.len();

        while self.buffered > self.capacity {
            let Some(front) = self.chunks.front_mut() else {
                break;
            };
            let excess = self.buffered - self.capacity;
            if front.len() <= excess {
                let dropped = front.len();
                self.chunks.pop_front();
                self.buffered -= dropped;
                self.tail += dropped as u64;
            } else {
                front.drain(..excess);
                self.buffered -= excess;
                self.tail += excess as u64;
            }
        }
    }

    /// Up to `limit` bytes from `offset`, or `None` when `offset` has aged out
    /// or is in the future.
    ///
    /// A future offset is not merely unserviceable, it means the peer and this
    /// session disagree about how much output exists — answer `None` and let
    /// the caller resynchronise from a snapshot rather than inventing bytes.
    ///
    /// The copy is bounded by `limit` and by what is actually new, so polling a
    /// caught-up client every few milliseconds costs nothing instead of a copy
    /// of the whole retained tail.
    pub(crate) fn since(&self, offset: u64, limit: usize) -> Option<Vec<u8>> {
        if offset > self.head || offset < self.tail {
            return None;
        }
        let wanted = usize::try_from(self.head - offset)
            .unwrap_or(usize::MAX)
            .min(limit);
        if wanted == 0 {
            return Some(Vec::new());
        }
        let mut skip = (offset - self.tail) as usize;
        let mut out = Vec::with_capacity(wanted);
        for chunk in &self.chunks {
            if skip >= chunk.len() {
                skip -= chunk.len();
                continue;
            }
            let take = (chunk.len() - skip).min(wanted - out.len());
            out.extend_from_slice(&chunk[skip..skip + take]);
            skip = 0;
            if out.len() == wanted {
                break;
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_from_an_offset_it_still_holds() {
        let mut ring = OutputRing::new(64);
        ring.push(b"hello ");
        ring.push(b"world");
        assert_eq!(ring.tail(), 0);
        assert_eq!(ring.head(), 11);
        assert_eq!(ring.since(0, usize::MAX).unwrap(), b"hello world");
        assert_eq!(ring.since(6, usize::MAX).unwrap(), b"world");
        assert_eq!(ring.since(11, usize::MAX).unwrap(), b"");
    }

    /// Pruning must advance `tail` by exactly the bytes dropped. If it did not,
    /// `since` would slice at the wrong place and hand the client a stream that
    /// starts mid-escape-sequence.
    #[test]
    fn pruning_advances_the_tail_by_the_bytes_dropped() {
        let mut ring = OutputRing::new(8);
        ring.push(b"aaaa");
        ring.push(b"bbbb");
        ring.push(b"cccc");
        assert_eq!(ring.head(), 12);
        assert_eq!(ring.tail(), 4);
        assert_eq!(ring.since(4, usize::MAX).unwrap(), b"bbbbcccc");
    }

    #[test]
    fn a_partially_dropped_chunk_keeps_its_remainder() {
        let mut ring = OutputRing::new(4);
        ring.push(b"abcdef");
        assert_eq!(ring.head(), 6);
        assert_eq!(ring.tail(), 2);
        assert_eq!(ring.since(2, usize::MAX).unwrap(), b"cdef");
    }

    /// The reattach decision depends on this: an aged-out offset must be
    /// refused, not silently served from whatever the ring happens to start
    /// with, or the client renders a corrupt screen and never knows.
    #[test]
    fn an_aged_out_offset_is_refused() {
        let mut ring = OutputRing::new(4);
        ring.push(b"aaaa");
        ring.push(b"bbbb");
        assert_eq!(ring.tail(), 4);
        assert!(ring.since(0, usize::MAX).is_none());
        assert!(ring.since(3, usize::MAX).is_none());
        assert!(ring.since(4, usize::MAX).is_some());
    }

    #[test]
    fn an_offset_past_the_head_is_refused() {
        let mut ring = OutputRing::new(64);
        ring.push(b"abc");
        assert!(ring.since(4, usize::MAX).is_none());
    }

    #[test]
    fn an_empty_write_does_not_move_the_offsets() {
        let mut ring = OutputRing::new(64);
        ring.push(b"abc");
        ring.push(b"");
        assert_eq!(ring.head(), 3);
        assert_eq!(ring.tail(), 0);
    }

    /// The pump polls every 8 ms. Copying the whole retained tail each time
    /// just to discover nothing is new, or to truncate it to the send budget,
    /// is what `limit` and the caught-up fast path exist to prevent.
    #[test]
    fn a_read_copies_only_what_is_new_and_no_more_than_the_limit() {
        let mut ring = OutputRing::new(1024 * 1024);
        ring.push(&vec![b'a'; 200_000]);
        let head = ring.head();
        assert_eq!(ring.since(head, usize::MAX).unwrap(), b"");
        let bounded = ring.since(0, 10).unwrap();
        assert_eq!(bounded, vec![b'a'; 10]);
        assert_eq!(bounded.capacity(), 10, "must not reserve the whole tail");
        assert_eq!(ring.since(head - 3, 10).unwrap().len(), 3);
    }

    #[test]
    fn tiny_writes_do_not_multiply_chunks() {
        let mut ring = OutputRing::new(RING_BYTES);
        for _ in 0..RING_BYTES {
            ring.push(b"x");
        }
        assert!(
            ring.chunks.len() <= RING_BYTES / CHUNK_TARGET + 2,
            "{} chunks for {} one-byte writes",
            ring.chunks.len(),
            RING_BYTES
        );
        assert_eq!(
            ring.since(ring.tail(), usize::MAX).unwrap().len(),
            RING_BYTES
        );
    }
}
