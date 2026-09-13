//! Reverse reachability over the index.

use fixedbitset::FixedBitSet;
use std::collections::VecDeque;

use crate::data::{IndexRef, NodeKind, NO_ID};
use crate::view::GraphView;

/// Options controlling which nodes reach the output.
#[derive(Clone, Debug, Default)]
pub struct WalkOptions {
    /// Keep `manual`-tagged rules in the result.
    pub include_manual: bool,
    /// Record a parent pointer per visited node so paths back to the direct set
    /// can be reconstructed (`--explain`). Off by default: it costs 4n bytes.
    pub explain: bool,
    /// Stop after this many hops from the seeds. `None` walks to the fixpoint;
    /// `Some(0)` reports only the seeds themselves.
    pub max_depth: Option<u32>,
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
    /// `depth[v]` is the number of hops at which `v` was first reached — 0 for
    /// a seed, `NO_ID` for a node the walk never visited. Every visited node
    /// counts as a hop, tombstones included: a target reached through a
    /// deleted dependency is one hop further than the deletion itself.
    pub depth: Vec<u32>,
    /// `package_depth[v]` is the number of package boundaries the walk
    /// crossed to first reach `v`: an edge inside one package is free, an
    /// edge into another package costs one. 0 for a seed and for anything
    /// reached without leaving the seed's package, `NO_ID` for a node the
    /// walk never visited. Bounded by `depth[v]`, and usually far below it.
    pub package_depth: Vec<u32>,
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

    /// Hops from the nearest seed, for a visited node.
    pub fn depth_of(&self, id: u32) -> Option<u32> {
        let d = *self.depth.get(id as usize)?;
        (d != NO_ID).then_some(d)
    }

    /// Package boundaries between the nearest seed and a visited node.
    pub fn package_depth_of(&self, id: u32) -> Option<u32> {
        let d = *self.package_depth.get(id as usize)?;
        (d != NO_ID).then_some(d)
    }
}

/// Breadth-first walk over the reverse edges from `direct`: everything that
/// depends on the seeds.
///
/// The queue is a plain FIFO over a bitset of visited ids, so a node is
/// enqueued at most once and the walk is O(affected + their in-edges) — it
/// never touches the rest of the graph. Terminates on cyclic input even though
/// Bazel's target graph is acyclic.
pub fn walk(index: IndexRef<'_>, direct: &[u32], opts: &WalkOptions) -> WalkResult {
    walk_view(&index, direct, opts)
}

/// The reverse walk over any [`GraphView`] — a complete index, or a base plus a
/// delta overlay.
///
/// One implementation, so the overlay cannot answer a different question from
/// the materialised index; the property tests check that it does not.
pub fn walk_view<V: GraphView + ?Sized>(
    view: &V,
    direct: &[u32],
    opts: &WalkOptions,
) -> WalkResult {
    bfs(view, &Reverse(view), direct, opts)
}

/// The same walk over the forward edges: everything the seeds depend on.
pub fn walk_deps(index: IndexRef<'_>, seeds: &[u32], opts: &WalkOptions) -> WalkResult {
    walk_edges(index, index.forward, seeds, opts)
}

/// Breadth-first walk over an arbitrary edge set of the index.
///
/// Both directions are the same traversal; which CSR is handed in decides
/// whether the question is "what depends on this" or "what does this depend
/// on". Keeping one implementation is what stops the two answers from drifting.
pub fn walk_edges(
    index: IndexRef<'_>,
    edges: crate::CsrRef<'_>,
    direct: &[u32],
    opts: &WalkOptions,
) -> WalkResult {
    bfs(&index, &edges, direct, opts)
}

/// Where a walk's edges come from. Same shape as [`GraphView::reverse_row`]:
/// a source that has the row contiguous hands back its own slice, one that has
/// to compute it fills `scratch`.
trait Rows {
    fn row<'s>(&'s self, u: u32, scratch: &'s mut Vec<u32>) -> &'s [u32];
}

/// The reverse edges of a [`GraphView`].
struct Reverse<'v, V: ?Sized>(&'v V);

impl<V: GraphView + ?Sized> Rows for Reverse<'_, V> {
    fn row<'s>(&'s self, u: u32, scratch: &'s mut Vec<u32>) -> &'s [u32] {
        self.0.reverse_row(u, scratch)
    }
}

/// Any CSR of the index, as handed to [`walk_edges`].
impl Rows for crate::CsrRef<'_> {
    fn row<'s>(&'s self, u: u32, _scratch: &'s mut Vec<u32>) -> &'s [u32] {
        crate::CsrRef::row(self, u)
    }
}

/// The one traversal behind every walk: node metadata comes from `view`, the
/// edges out of a node from `rows`.
fn bfs<V: GraphView + ?Sized, R: Rows + ?Sized>(
    view: &V,
    rows: &R,
    direct: &[u32],
    opts: &WalkOptions,
) -> WalkResult {
    let n = view.num_nodes() as usize;
    let mut visited = FixedBitSet::with_capacity(n);
    let mut parent = opts.explain.then(|| vec![NO_ID; n]);
    let mut depth = vec![NO_ID; n];
    let mut queue: VecDeque<u32> = VecDeque::with_capacity(direct.len().max(16));
    let mut scratch: Vec<u32> = Vec::new();

    for &d in direct {
        if (d as usize) < n && !visited.put(d as usize) {
            depth[d as usize] = 0;
            queue.push_back(d);
        }
    }

    while let Some(u) = queue.pop_front() {
        let du = depth[u as usize];
        if opts.max_depth.is_some_and(|m| du >= m) {
            continue;
        }
        for &v in rows.row(u, &mut scratch) {
            if (v as usize) < n && !visited.put(v as usize) {
                if let Some(p) = parent.as_mut() {
                    p[v as usize] = u;
                }
                depth[v as usize] = du + 1;
                queue.push_back(v);
            }
        }
    }

    let package_depth = package_bfs(view, rows, direct, &visited, n);

    let mut affected = Vec::new();
    let mut tests = Vec::new();
    for id in visited.ones() {
        let id = id as u32;
        if view.kind_of(id) != NodeKind::Rule || view.is_tombstone(id) {
            continue;
        }
        if !opts.include_manual && view.is_manual(id) {
            continue;
        }
        affected.push(id);
        if view.is_test(id) {
            tests.push(id);
        }
    }

    WalkResult {
        visited,
        affected,
        tests,
        parent,
        depth,
        package_depth,
    }
}

/// The package part of a label, repository included: `@x//a/b:c` -> `@x//a/b`.
/// Two nodes are in the same package when this compares equal.
fn package_key(label: &str) -> &str {
    match label.rfind(':') {
        Some(i) => &label[..i],
        None => label,
    }
}

/// The second distance: a 0-1 breadth-first walk over the nodes the first one
/// visited, where an edge that stays inside a package costs nothing and one
/// that leaves it costs one. Staying inside `visited` keeps `max_depth` and
/// the seed set the same for both distances, so the two numbers on a target
/// describe the same walk.
fn package_bfs<V: GraphView + ?Sized, R: Rows + ?Sized>(
    view: &V,
    rows: &R,
    direct: &[u32],
    visited: &FixedBitSet,
    n: usize,
) -> Vec<u32> {
    let mut dist = vec![NO_ID; n];
    let mut queue: VecDeque<u32> = VecDeque::with_capacity(direct.len().max(16));
    let mut scratch: Vec<u32> = Vec::new();
    for &d in direct {
        if (d as usize) < n && dist[d as usize] != 0 {
            dist[d as usize] = 0;
            queue.push_back(d);
        }
    }
    while let Some(u) = queue.pop_front() {
        let du = dist[u as usize];
        let pu = package_key(view.label(u));
        for &v in rows.row(u, &mut scratch) {
            if (v as usize) >= n || !visited.contains(v as usize) {
                continue;
            }
            let dv = du + u32::from(package_key(view.label(v)) != pu);
            if dv < dist[v as usize] {
                dist[v as usize] = dv;
                if dv == du {
                    queue.push_front(v);
                } else {
                    queue.push_back(v);
                }
            }
        }
    }
    dist
}

/// Sorts ids by label. Output ordering is part of the contract: same inputs,
/// byte-identical output.
pub fn sort_by_label(index: IndexRef<'_>, ids: &mut [u32]) {
    ids.sort_unstable_by(|&a, &b| index.label(a).cmp(index.label(b)));
}
