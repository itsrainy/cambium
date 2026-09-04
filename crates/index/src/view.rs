//! One interface over the two shapes a graph can have here.
//!
//! An [`IndexRef`] is a complete index; an [`Overlay`](crate::overlay::Overlay)
//! is a base plus a delta. The walk, and everything that reads a result out of
//! it, work over either — which is what makes "the overlay answers the same
//! question the materialised index would" a property the type system helps
//! with rather than a convention.

use crate::data::{IndexRef, NodeKind};

pub trait GraphView {
    fn num_nodes(&self) -> u32;
    fn label(&self, id: u32) -> &str;
    fn kind_of(&self, id: u32) -> NodeKind;
    fn is_test(&self, id: u32) -> bool;
    fn is_manual(&self, id: u32) -> bool;
    fn is_tombstone(&self, id: u32) -> bool;

    /// The nodes that depend on `id`.
    ///
    /// Takes a scratch buffer so an implementation that has to compute the row
    /// can do so without allocating per row, while one that already has it
    /// contiguous can hand back its own slice.
    fn reverse_row<'s>(&'s self, id: u32, scratch: &'s mut Vec<u32>) -> &'s [u32];

    fn id_of(&self, label: &str) -> Option<u32>;

    fn is_rule(&self, id: u32) -> bool {
        self.kind_of(id) == NodeKind::Rule
    }
}

impl GraphView for IndexRef<'_> {
    fn num_nodes(&self) -> u32 {
        IndexRef::num_nodes(self)
    }

    fn label(&self, id: u32) -> &str {
        IndexRef::label(self, id)
    }

    fn kind_of(&self, id: u32) -> NodeKind {
        IndexRef::kind_of(self, id)
    }

    fn is_test(&self, id: u32) -> bool {
        self.is_test[id as usize]
    }

    fn is_manual(&self, id: u32) -> bool {
        IndexRef::is_manual(self, id)
    }

    fn is_tombstone(&self, id: u32) -> bool {
        IndexRef::is_tombstone(self, id)
    }

    fn reverse_row<'s>(&'s self, id: u32, _scratch: &'s mut Vec<u32>) -> &'s [u32] {
        self.reverse.row(id)
    }

    fn id_of(&self, label: &str) -> Option<u32> {
        IndexRef::id_of(self, label)
    }
}

/// Labels for `ids`, sorted and deduplicated. Output ordering is part of the
/// contract: same inputs, byte-identical output.
pub fn labels_sorted<V: GraphView + ?Sized>(view: &V, ids: &[u32]) -> Vec<String> {
    let mut out: Vec<String> = ids.iter().map(|&i| view.label(i).to_string()).collect();
    out.sort_unstable();
    out.dedup();
    out
}
