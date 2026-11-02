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

#[cfg(test)]
mod tests {
    #[test]
    fn below_is_in_range() {
        assert!(super::below(3).is_some_and(|i| i < 3));
        assert_eq!(super::below(0), None);
    }
}
