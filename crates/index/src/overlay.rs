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
use crate::space::{LabelSpace, NodeSpace};
use crate::view::GraphView;

/// A base index plus one generation of changes, without the copy.
///
/// The labels and node columns are the builder's own [`LabelSpace`] and
/// [`NodeSpace`], handed over as they were: the overlay reads them exactly the
/// way the splice that produced it did.
pub struct Overlay<'a> {
    pub(crate) base: Option<IndexRef<'a>>,
    pub(crate) labels: LabelSpace<'a>,
    pub(crate) nodes: NodeSpace<'a>,
    /// Consumers gained, by consumed node.
    pub(crate) rev_added: FxHashMap<u32, Vec<u32>>,
    /// Consumers lost, by consumed node.
    pub(crate) rev_removed: FxHashMap<u32, FxHashSet<u32>>,
}

impl Overlay<'_> {
    /// How much the overlay is carrying, for reporting and for tests that care
    /// that it stayed small.
    pub fn delta_size(&self) -> OverlaySize {
        OverlaySize {
            new_nodes: self.nodes.appended().len(),
            changed_nodes: self.nodes.overridden().len(),
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
        self.nodes.len()
    }

    fn label(&self, id: u32) -> &str {
        self.labels.get(id)
    }

    fn kind_of(&self, id: u32) -> NodeKind {
        NodeKind::from_u8(self.nodes.get(id).kind)
    }

    fn is_test(&self, id: u32) -> bool {
        self.nodes.get(id).is_test
    }

    fn is_manual(&self, id: u32) -> bool {
        self.nodes.get(id).flags & flags::MANUAL != 0
    }

    fn is_tombstone(&self, id: u32) -> bool {
        self.nodes.get(id).flags & flags::TOMBSTONE != 0
    }

    fn reverse_row<'s>(&'s self, id: u32, scratch: &'s mut Vec<u32>) -> &'s [u32] {
        let base: &[u32] = match self.base {
            Some(b) if id < self.nodes.base_len() => b.reverse.row(id),
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
        self.labels.lookup(label)
    }
}
