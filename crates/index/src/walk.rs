//! Reverse reachability over the index.

use fixedbitset::FixedBitSet;
use std::collections::VecDeque;

use crate::data::{IndexRef, NodeKind, NO_ID};

/// Options controlling which nodes reach the output.
#[derive(Clone, Debug, Default)]
pub struct WalkOptions {
    /// Keep `manual`-tagged rules in the result.
    pub include_manual: bool,
    /// Record a parent pointer per visited node so paths back to the direct set
    /// can be reconstructed (`--explain`). Off by default: it costs 4n bytes.
    pub explain: bool,
}

/// Result of a reverse walk.
pub struct WalkResult {
    /// Every visited id, including the seeds and non-rule nodes.
    pub visited: FixedBitSet,
    /// Affected rule ids, ascending.
    pub affected: Vec<u32>,
    /// Affected test rule ids, ascending.
    pub tests: Vec<u32>,
    /// `parent[v]` is the node that first reached `v`, or `NO_ID` for seeds.
    pub parent: Option<Vec<u32>>,
}

impl WalkResult {
    /// Shortest path (in BFS hops) from `id` back to the direct-set member that
    /// reached it. Requires `WalkOptions::explain`.
    pub fn path_to_seed(&self, mut id: u32) -> Option<Vec<u32>> {
        let parent = self.parent.as_ref()?;
        if !self.visited.contains(id as usize) {
            return None;
        }
        let mut path = vec![id];
        while parent[id as usize] != NO_ID {
            id = parent[id as usize];
            path.push(id);
            if path.len() > parent.len() {
                return None; // defensive: cycles cannot happen but never spin
            }
        }
        Some(path)
    }
}

/// Breadth-first walk over the reverse edges from `direct`.
///
/// The queue is a plain FIFO over a bitset of visited ids, so a node is
/// enqueued at most once and the walk is O(affected + their in-edges) — it
/// never touches the rest of the graph. Terminates on cyclic input even though
/// Bazel's target graph is acyclic.
pub fn walk(index: IndexRef<'_>, direct: &[u32], opts: &WalkOptions) -> WalkResult {
    let n = index.num_nodes() as usize;
    let mut visited = FixedBitSet::with_capacity(n);
    let mut parent = opts.explain.then(|| vec![NO_ID; n]);
    let mut queue: VecDeque<u32> = VecDeque::with_capacity(direct.len().max(16));

    for &d in direct {
        if (d as usize) < n && !visited.put(d as usize) {
            queue.push_back(d);
        }
    }

    while let Some(u) = queue.pop_front() {
        for &v in index.reverse.row(u) {
            if (v as usize) < n && !visited.put(v as usize) {
                if let Some(p) = parent.as_mut() {
                    p[v as usize] = u;
                }
                queue.push_back(v);
            }
        }
    }

    let mut affected = Vec::new();
    let mut tests = Vec::new();
    for id in visited.ones() {
        let id = id as u32;
        if index.kind_of(id) != NodeKind::Rule || index.is_tombstone(id) {
            continue;
        }
        if !opts.include_manual && index.is_manual(id) {
            continue;
        }
        affected.push(id);
        if index.is_test[id as usize] {
            tests.push(id);
        }
    }

    WalkResult {
        visited,
        affected,
        tests,
        parent,
    }
}

/// Sorts ids by label. Output ordering is part of the contract: same inputs,
/// byte-identical output.
pub fn sort_by_label(index: IndexRef<'_>, ids: &mut [u32]) {
    ids.sort_unstable_by(|&a, &b| index.label(a).cmp(index.label(b)));
}

/// Labels for `ids`, sorted.
pub fn labels_sorted(index: IndexRef<'_>, ids: &[u32]) -> Vec<String> {
    let mut out: Vec<String> = ids.iter().map(|&i| index.label(i).to_string()).collect();
    out.sort_unstable();
    out.dedup();
    out
}
