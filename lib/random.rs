// Kernel randomness (getrandom): a few retries at most, never a fallback to anything predictable.

/// getrandom until `buf` is full: a signal or a short read is retried, a few times at most.
pub fn fill(buf: &mut [u8]) -> bool {
    let mut off = 0;
    for _ in 0..8 {
        let n = unsafe { libc::getrandom(buf[off..].as_mut_ptr().cast(), buf.len() - off, 0) };
        if n > 0 {
            off += n as usize;
            if off == buf.len() {
                return true;
            }
        } else if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
    false
}

/// Uniform in 0..n (rejection sampling, no modulo bias); None if the kernel gives no randomness.
pub fn below(n: u64) -> Option<u64> {
    if n == 0 {
        return None;
    }
    // the largest multiple of n: values from it up are rejected (less than half the range)
    let limit = u64::MAX - u64::MAX % n;
    for _ in 0..16 {
        let mut b = [0u8; 8];
        if !fill(&mut b) {
            return None;
        }
        let x = u64::from_ne_bytes(b);
        if x < limit {
            return Some(x % n);
        }
    }
    None
}

/// Small uniform picks (secrets: password characters) from one getrandom buffer, refilled as
/// rejection sampling uses it up; zeroed on drop.
pub struct Rng {
    buf: [u8; 256],
    pos: usize,
}

impl Default for Rng {
    fn default() -> Rng {
        Rng { buf: [0; 256], pos: 256 }
    }
}

impl Rng {
    fn byte(&mut self) -> Option<u8> {
        if self.pos == self.buf.len() {
            if !fill(&mut self.buf) {
                return None;
            }
            self.pos = 0;
        }
        self.pos += 1;
        Some(self.buf[self.pos - 1])
    }

    /// Uniform in 0..n for 1 <= n <= 256 (bytes past the last whole multiple of n are rejected);
    /// None if the kernel gives no randomness.
    pub fn below(&mut self, n: usize) -> Option<usize> {
        if n == 0 || n > 256 {
            return None;
        }
        let limit = 256 - 256 % n;
        loop {
            let b = self.byte()? as usize;
            if b < limit {
                return Some(b % n);
            }
        }
    }
}

impl Drop for Rng {
    fn drop(&mut self) {
        for b in &mut self.buf {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn below_is_in_range() {
        assert!(super::below(3).is_some_and(|i| i < 3));
        assert_eq!(super::below(0), None);
    }

    #[test]
    fn rng_below_is_in_range() {
        let mut r = super::Rng::default();
        assert!((0..1000).all(|_| r.below(7).is_some_and(|i| i < 7)));
        assert_eq!(r.below(256).map(|i| i < 256), Some(true));
        assert_eq!(r.below(0), None);
    }
}
