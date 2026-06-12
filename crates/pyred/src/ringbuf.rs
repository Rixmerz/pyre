//! Fixed-capacity circular byte buffer for PTY output replay.
//!
//! `RingBuf` stores up to `cap` bytes of raw PTY output. When capacity is
//! exceeded the oldest bytes are evicted. `snapshot` returns a linear copy
//! oldest→newest suitable for replay on reattach.

use bytes::Bytes;

/// Fixed-capacity circular byte buffer.
pub struct RingBuf {
    buf: Vec<u8>,
    /// Index of the oldest byte when the buffer is full.
    head: usize,
    /// Number of valid bytes currently stored.
    len: usize,
    cap: usize,
}

impl RingBuf {
    /// Create a new ring buffer with the given byte capacity.
    pub fn new(cap: usize) -> Self {
        Self {
            buf: vec![0u8; cap],
            head: 0,
            len: 0,
            cap,
        }
    }

    /// Append `bytes` to the buffer, evicting oldest bytes as needed.
    ///
    /// If `bytes.len() >= cap`, only the last `cap` bytes of the input are
    /// retained.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.cap == 0 || bytes.is_empty() {
            return;
        }

        // If incoming is larger than the ring, only keep the tail.
        let bytes = if bytes.len() >= self.cap {
            &bytes[bytes.len() - self.cap..]
        } else {
            bytes
        };

        for &b in bytes {
            let write_pos = (self.head + self.len) % self.cap;
            self.buf[write_pos] = b;
            if self.len < self.cap {
                self.len += 1;
            } else {
                // Overwrite oldest: advance head.
                self.head = (self.head + 1) % self.cap;
            }
        }
    }

    /// Return a linearized copy of the buffer contents, oldest→newest.
    // dead_code: snapshot() is called today by server.rs (replay/capture_pane)
    // and worker.rs (capture_pane); the attribute suppresses the lint only for
    // targets that don't see those call sites (e.g. lib.rs integration tests).
    #[allow(dead_code)]
    pub fn snapshot(&self) -> Bytes {
        if self.len == 0 {
            return Bytes::new();
        }
        let mut out = Vec::with_capacity(self.len);
        if self.head + self.len <= self.cap {
            out.extend_from_slice(&self.buf[self.head..self.head + self.len]);
        } else {
            out.extend_from_slice(&self.buf[self.head..]);
            out.extend_from_slice(&self.buf[..self.len - (self.cap - self.head)]);
        }
        Bytes::from(out)
    }

    /// Number of bytes currently stored.
    // dead_code: used in unit tests below; the attribute is needed because
    // the lib target and non-test binary builds do not see the test call site.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the buffer contains no bytes.
    // dead_code: utility predicate; used in tests. Kept for future callers
    // that need an emptiness check without calling snapshot().
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Maximum capacity in bytes.
    // dead_code: diagnostic / test helper; no production caller yet.
    #[allow(dead_code)]
    pub fn cap(&self) -> usize {
        self.cap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_zero_bytes_is_empty() {
        let mut rb = RingBuf::new(64);
        rb.push(&[]);
        assert_eq!(rb.len(), 0);
        assert_eq!(rb.snapshot(), Bytes::new());
    }

    #[test]
    fn push_less_than_cap() {
        let mut rb = RingBuf::new(64);
        rb.push(b"hello");
        assert_eq!(rb.len(), 5);
        assert_eq!(rb.snapshot().as_ref(), b"hello");
    }

    #[test]
    fn push_larger_than_cap_single_call() {
        let cap = 8;
        let mut rb = RingBuf::new(cap);
        let data = b"abcdefghijklmnop"; // 16 bytes > cap
        rb.push(data);
        assert_eq!(rb.len(), cap);
        assert_eq!(rb.snapshot().as_ref(), &data[data.len() - cap..]);
    }

    #[test]
    fn multiple_pushes_wrapping_correct_order() {
        let mut rb = RingBuf::new(8);
        rb.push(b"12345678"); // fills exactly
        rb.push(b"ABCD"); // wraps: evicts "1234", keeps "5678ABCD"
        let snap = rb.snapshot();
        assert_eq!(snap.as_ref(), b"5678ABCD");
    }

    #[test]
    fn exactly_cap_bytes_pushed() {
        let cap = 10;
        let mut rb = RingBuf::new(cap);
        let data = b"0123456789";
        rb.push(data);
        assert_eq!(rb.len(), cap);
        assert_eq!(rb.snapshot().as_ref(), data.as_ref());
    }
}
