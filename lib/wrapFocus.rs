/// Index after moving by `delta` with wrap-around; -1 (nothing selected) moving up lands on the last.
pub fn wrap_index(i: i64, delta: i64, len: i64) -> i64 {
    ((if i < 0 && delta < 0 { 0 } else { i }) + delta + len) % len
}

#[cfg(test)]
mod tests {
    use super::wrap_index;

    #[test]
    fn wraps_both_ways() {
        assert_eq!(wrap_index(0, -1, 3), 2);
        assert_eq!(wrap_index(2, 1, 3), 0);
        assert_eq!(wrap_index(-1, 1, 3), 0);
        assert_eq!(wrap_index(-1, -1, 3), 2);
    }
}
