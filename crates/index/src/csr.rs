//! Compressed sparse row edge storage.
//!
//! Rows are always sorted ascending and deduplicated. That is what makes two
//! index generations comparable row-for-row, and it is what a future
//! merge-based incremental reverse rebuild would rely on.

use rayon::prelude::*;

/// Borrowed CSR: `offsets` has length `n + 1`, `edges` is the flat edge array.
#[derive(Clone, Copy)]
pub struct CsrRef<'a> {
    pub offsets: &'a [u32],
    pub edges: &'a [u32],
}

impl<'a> CsrRef<'a> {
    pub const EMPTY: CsrRef<'static> = CsrRef {
        offsets: &[0],
        edges: &[],
    };

    #[inline]
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn num_edges(&self) -> usize {
        self.edges.len()
    }

    /// Edges out of node `u`. Empty for out-of-range ids so callers do not need
    /// to bounds-check ids that came from a different index generation.
    #[inline]
    pub fn row(&self, u: u32) -> &'a [u32] {
        let i = u as usize;
        if i + 1 >= self.offsets.len() {
            return &[];
        }
        let s = self.offsets[i] as usize;
        let e = self.offsets[i + 1] as usize;
        &self.edges[s..e]
    }

    #[inline]
    pub fn row_len(&self, u: u32) -> usize {
        let i = u as usize;
        if i + 1 >= self.offsets.len() {
            return 0;
        }
        (self.offsets[i + 1] - self.offsets[i]) as usize
    }
}

/// Owned CSR.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Csr {
    pub offsets: Vec<u32>,
    pub edges: Vec<u32>,
}

impl Csr {
    pub fn empty(n: usize) -> Self {
        Self {
            offsets: vec![0; n + 1],
            edges: Vec::new(),
        }
    }

    pub fn as_ref(&self) -> CsrRef<'_> {
        CsrRef {
            offsets: &self.offsets,
            edges: &self.edges,
        }
    }

    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn row(&self, u: u32) -> &[u32] {
        self.as_ref().row(u)
    }

    /// Builds a CSR over `n` nodes from `(src, dst)` pairs. Pairs are sorted and
    /// deduplicated in place.
    pub fn from_pairs(n: usize, mut pairs: Vec<(u32, u32)>) -> Self {
        sort_dedup_pairs(&mut pairs);
        let mut offsets = vec![0u32; n + 1];
        for &(s, _) in &pairs {
            offsets[s as usize + 1] += 1;
        }
        prefix_sum(&mut offsets);
        let edges: Vec<u32> = pairs.into_iter().map(|(_, d)| d).collect();
        Self { offsets, edges }
    }

    /// Rebuilds the reverse of `fwd` over the same `n`-node id space.
    ///
    /// Two-pass counting sort. The scatter pass walks sources in increasing
    /// order, which leaves every reverse row sorted ascending for free.
    pub fn reverse_of(fwd: CsrRef<'_>, n: usize) -> Self {
        let mut offsets = vec![0u32; n + 1];
        for &d in fwd.edges {
            if (d as usize) < n {
                offsets[d as usize + 1] += 1;
            }
        }
        prefix_sum(&mut offsets);
        let total = offsets[n] as usize;
        let mut edges = vec![0u32; total];
        let mut cursor: Vec<u32> = offsets[..n].to_vec();
        for u in 0..fwd.len().min(n) as u32 {
            for &d in fwd.row(u) {
                if (d as usize) < n {
                    let slot = &mut cursor[d as usize];
                    edges[*slot as usize] = u;
                    *slot += 1;
                }
            }
        }
        Self { offsets, edges }
    }

    /// Parallel variant of [`Csr::reverse_of`] for large graphs. Rows come out
    /// unordered from the atomic scatter, so each row is sorted afterwards.
    pub fn reverse_of_parallel(fwd: CsrRef<'_>, n: usize) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};

        let counts: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();
        fwd.edges.par_iter().for_each(|&d| {
            if (d as usize) < n {
                counts[d as usize].fetch_add(1, Ordering::Relaxed);
            }
        });
        let mut offsets = vec![0u32; n + 1];
        for i in 0..n {
            offsets[i + 1] = counts[i].load(Ordering::Relaxed);
        }
        prefix_sum(&mut offsets);
        let total = offsets[n] as usize;

        let cursor: Vec<AtomicU32> = offsets[..n].iter().map(|&o| AtomicU32::new(o)).collect();
        let mut edges = vec![0u32; total];
        {
            // Each slot is claimed exactly once via fetch_add, so the writes are
            // disjoint; the atomics are only used to hand out the slots.
            let slots = edges.as_mut_ptr() as usize;
            (0..fwd.len().min(n) as u32)
                .into_par_iter()
                .for_each(|u| {
                    for &d in fwd.row(u) {
                        if (d as usize) < n {
                            let at = cursor[d as usize].fetch_add(1, Ordering::Relaxed) as usize;
                            unsafe { *(slots as *mut u32).add(at) = u };
                        }
                    }
                });
        }
        let mut out = Self { offsets, edges };
        out.sort_rows_parallel();
        out
    }

    /// Sorts and deduplicates every row in parallel. Offsets are left untouched,
    /// so this is only valid when rows are known to be duplicate-free (the
    /// reverse of a deduplicated forward CSR is).
    fn sort_rows_parallel(&mut self) {
        let offsets = std::mem::take(&mut self.offsets);
        {
            let mut rest: &mut [u32] = &mut self.edges;
            let mut chunks: Vec<&mut [u32]> = Vec::with_capacity(offsets.len());
            for w in offsets.windows(2) {
                let len = (w[1] - w[0]) as usize;
                let (head, tail) = rest.split_at_mut(len);
                chunks.push(head);
                rest = tail;
            }
            chunks.par_iter_mut().for_each(|c| c.sort_unstable());
        }
        self.offsets = offsets;
    }

    /// Total bytes of the two arrays, for reporting.
    pub fn byte_size(&self) -> usize {
        self.offsets.len() * 4 + self.edges.len() * 4
    }
}

#[inline]
pub(crate) fn prefix_sum(offsets: &mut [u32]) {
    let mut acc = 0u32;
    for o in offsets.iter_mut() {
        acc += *o;
        *o = acc;
    }
}

/// Sorts by `(src, dst)` and removes duplicate edges.
pub fn sort_dedup_pairs(pairs: &mut Vec<(u32, u32)>) {
    if pairs.len() > 1 << 16 {
        pairs.par_sort_unstable();
    } else {
        pairs.sort_unstable();
    }
    pairs.dedup();
}

/// Sorts and deduplicates a single row.
#[inline]
pub fn sort_dedup_row(row: &mut Vec<u32>) {
    row.sort_unstable();
    row.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive_reverse(fwd: &Csr, n: usize) -> Vec<Vec<u32>> {
        let mut r = vec![Vec::new(); n];
        for u in 0..fwd.len() as u32 {
            for &d in fwd.row(u) {
                r[d as usize].push(u);
            }
        }
        for row in r.iter_mut() {
            row.sort_unstable();
        }
        r
    }

    fn sample() -> Csr {
        Csr::from_pairs(
            6,
            vec![(0, 1), (0, 2), (0, 1), (1, 3), (2, 3), (3, 5), (4, 5)],
        )
    }

    #[test]
    fn from_pairs_sorts_and_dedups() {
        let c = sample();
        assert_eq!(c.row(0), &[1, 2]);
        assert_eq!(c.row(5), &[] as &[u32]);
        assert_eq!(c.edges.len(), 6);
    }

    #[test]
    fn reverse_matches_naive() {
        let c = sample();
        let r = Csr::reverse_of(c.as_ref(), 6);
        let n = naive_reverse(&c, 6);
        for u in 0..6u32 {
            assert_eq!(r.row(u), n[u as usize].as_slice(), "row {u}");
        }
    }

    #[test]
    fn parallel_reverse_matches_serial() {
        let mut pairs = Vec::new();
        for i in 0..5000u32 {
            for k in 1..7u32 {
                pairs.push((i, (i * k + 13) % 5000));
            }
        }
        let c = Csr::from_pairs(5000, pairs);
        let a = Csr::reverse_of(c.as_ref(), 5000);
        let b = Csr::reverse_of_parallel(c.as_ref(), 5000);
        assert_eq!(a, b);
    }

    #[test]
    fn row_of_unknown_id_is_empty() {
        let c = sample();
        assert!(c.as_ref().row(99).is_empty());
    }
}
