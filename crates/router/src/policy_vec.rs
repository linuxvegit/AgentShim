//! Typed length-invariant wrapper around `Vec<T>`. Plan v0.6.1 P03 (M-7).
//!
//! `PolicyVec<T>` carries a `Vec<T>` plus a `paired_len: usize` that the
//! constructor enforces equal to `vec.len()` at construction time. The
//! internal callers in `resilient_caller.rs` use this to thread the
//! "one policy per chain element" invariant through the type system,
//! replacing the v0.6.0 `.truncate()` + `debug_assert_eq!` pattern.
//!
//! T1 ships the wrapper alone; the `dead_code` allow is removed when
//! T2-T5 plumb it through `resilient_caller.rs`.
#![allow(dead_code)]

use thiserror::Error;

/// Error returned by [`PolicyVec::new`] when the supplied vector's
/// length doesn't match the expected paired length.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("policy vector length mismatch: got {actual}, expected {expected}")]
pub(crate) struct PolicyVecLenError {
    pub actual: usize,
    pub expected: usize,
}

/// A `Vec<T>` whose length matches the chain it pairs with by
/// construction. Internal to the router crate.
#[derive(Debug, Clone)]
pub(crate) struct PolicyVec<T> {
    inner: Vec<T>,
}

impl<T> PolicyVec<T> {
    /// Construct, enforcing `items.len() == expected_len`. Returns
    /// [`PolicyVecLenError`] otherwise.
    pub fn new(items: Vec<T>, expected_len: usize) -> Result<Self, PolicyVecLenError> {
        if items.len() != expected_len {
            return Err(PolicyVecLenError {
                actual: items.len(),
                expected: expected_len,
            });
        }
        Ok(PolicyVec { inner: items })
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn as_slice(&self) -> &[T] {
        &self.inner
    }

    /// Shorten the inner vector to its first `n` elements, preserving the
    /// length-paired invariant when the caller is also shortening the
    /// paired chain to the same `n`.
    pub fn retain_first(&mut self, n: usize) {
        self.inner.truncate(n);
    }

    /// Drop the wrapper; return the inner `Vec`.
    pub fn into_inner(self) -> Vec<T> {
        self.inner
    }
}

impl<T> std::ops::Index<usize> for PolicyVec<T> {
    type Output = T;
    fn index(&self, i: usize) -> &T {
        &self.inner[i]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_accepts_matched_length() {
        let v = PolicyVec::new(vec![1u8, 2, 3], 3).expect("matched len");
        assert_eq!(v.as_slice(), &[1, 2, 3]);
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn new_rejects_mismatched_length() {
        let err = PolicyVec::new(vec![1u8, 2, 3], 4).expect_err("len mismatch");
        match err {
            PolicyVecLenError { actual, expected } => {
                assert_eq!(actual, 3);
                assert_eq!(expected, 4);
            }
        }
    }

    #[test]
    fn retain_first_shortens_in_place_preserving_invariant() {
        let mut v = PolicyVec::new(vec![1u8, 2, 3], 3).unwrap();
        v.retain_first(2);
        assert_eq!(v.as_slice(), &[1, 2]);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn retain_first_no_op_when_count_equals_len() {
        let mut v = PolicyVec::new(vec![1u8, 2, 3], 3).unwrap();
        v.retain_first(3);
        assert_eq!(v.as_slice(), &[1, 2, 3]);
    }

    #[test]
    fn into_inner_returns_the_vec() {
        let v = PolicyVec::new(vec![1u8, 2, 3], 3).unwrap();
        assert_eq!(v.into_inner(), vec![1, 2, 3]);
    }
}
