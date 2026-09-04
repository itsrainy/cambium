//! The delta overlay: a spliced graph that was never materialised.
//!
//! A pull-request run does not need a new index. It needs to answer one
//! question — what does the reverse closure of the direct set look like *after*
//! the dirty packages are re-queried — and then throw everything away. Building
//! a complete new index to answer it copies hundreds of megabytes that nothing
//! will ever read.
//!
//! An [`Overlay`] is the base index plus what changed: the labels and metadata
//! of nodes this generation added or touched, and the reverse-edge corrections
//! implied by the forward rows it replaced. Reading a reverse row means taking
//! the base's and applying the correction, which is a hash lookup on a map with
//! a few hundred entries.
//!
//! `baseline update` still materialises: its output is what gets published, and
//! "a spliced index equals a full rebuild" is the property the correctness
//! oracles check. The two must agree, and
//! `crates/engine/tests/incremental.rs` is what enforces it — every synthetic
//! revision computes the answer both ways and requires them to be identical.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::data::{flags, IndexRef, NodeKind};
use crate::space::NodeMeta;
use crate::strtab::{Interner, StrTabRef};
use crate::view::GraphView;

/// A base index plus one generation of changes, without the copy.
pub struct Overlay<'a> {
    pub(crate) base: Option<IndexRef<'a>>,
    pub(crate) base_labels: Option<StrTabRef<'a>>,
    pub(crate) base_sorted: &'a [u32],
    pub(crate) base_n: u32,
    /// Labels appended this generation, indexed by `id - base_n`.
    pub(crate) new_labels: Interner,
    /// Metadata for appended ids.
    pub(crate) new_meta: Vec<NodeMeta>,
    /// Metadata for base ids this generation changed.
    pub(crate) over_meta: FxHashMap<u32, NodeMeta>,
    /// Consumers gained, by consumed node.
    pub(crate) rev_added: FxHashMap<u32, Vec<u32>>,
    /// Consumers lost, by consumed node.
    pub(crate) rev_removed: FxHashMap<u32, FxHashSet<u32>>,
}

impl<'a> Overlay<'a> {
    #[inline]
    fn meta(&self, id: u32) -> NodeMeta {
        if id >= self.base_n {
            return self.new_meta[(id - self.base_n) as usize];
        }
        if let Some(m) = self.over_meta.get(&id) {
            return *m;
        }
        let b = self.base.expect("base ids need a base view");
        let i = id as usize;
        NodeMeta {
            kind: b.kind[i],
            package: b.package[i],
            class: b.class[i],
            hash: b.hash[i],
            is_test: b.is_test[i],
            flags: b.flags[i],
        }
    }

    /// How much the overlay is carrying, for reporting and for tests that care
    /// that it stayed small.
    pub fn delta_size(&self) -> OverlaySize {
        OverlaySize {
            new_nodes: self.new_meta.len(),
            changed_nodes: self.over_meta.len(),
            rows_gaining_edges: self.rev_added.len(),
            rows_losing_edges: self.rev_removed.len(),
            edges_added: self.rev_added.values().map(|v| v.len()).sum(),
            edges_removed: self.rev_removed.values().map(|v| v.len()).sum(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OverlaySize {
    pub new_nodes: usize,
    pub changed_nodes: usize,
    pub rows_gaining_edges: usize,
    pub rows_losing_edges: usize,
    pub edges_added: usize,
    pub edges_removed: usize,
}

impl GraphView for Overlay<'_> {
    fn num_nodes(&self) -> u32 {
        self.base_n + self.new_meta.len() as u32
    }

    fn label(&self, id: u32) -> &str {
        if id < self.base_n {
            self.base_labels
                .expect("base ids need a base table")
                .get(id)
        } else {
            self.new_labels.get(id - self.base_n)
        }
    }

    fn kind_of(&self, id: u32) -> NodeKind {
        NodeKind::from_u8(self.meta(id).kind)
    }

    fn is_test(&self, id: u32) -> bool {
        self.meta(id).is_test
    }

    fn is_manual(&self, id: u32) -> bool {
        self.meta(id).flags & flags::MANUAL != 0
    }

    fn is_tombstone(&self, id: u32) -> bool {
        self.meta(id).flags & flags::TOMBSTONE != 0
    }

    fn reverse_row<'s>(&'s self, id: u32, scratch: &'s mut Vec<u32>) -> &'s [u32] {
        let base: &[u32] = match self.base {
            Some(b) if id < self.base_n => b.reverse.row(id),
            _ => &[],
        };
        let added = self.rev_added.get(&id);
        let removed = self.rev_removed.get(&id);
        if added.is_none() && removed.is_none() {
            // The overwhelmingly common case: this row is untouched, so hand
            // back the base's slice without copying it.
            return base;
        }
        scratch.clear();
        match removed {
            Some(r) => scratch.extend(base.iter().copied().filter(|u| !r.contains(u))),
            None => scratch.extend_from_slice(base),
        }
        if let Some(a) = added {
            scratch.extend_from_slice(a);
        }
        scratch.sort_unstable();
        scratch.dedup();
        scratch
    }

    fn id_of(&self, label: &str) -> Option<u32> {
        if let Some(base) = self.base_labels {
            if let Ok(i) = self
                .base_sorted
                .binary_search_by(|&id| base.get(id).cmp(label))
            {
                return Some(self.base_sorted[i]);
            }
        }
        self.new_labels.lookup(label).map(|i| i + self.base_n)
    }
}
