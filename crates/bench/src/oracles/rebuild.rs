//! The incremental-equals-full oracle on real input.
//!
//! After a walk has brought an index forward commit by commit, a cold build
//! at the same commit must describe the same graph. Ids are not comparable
//! across lineages, so the comparison is by label: the live nodes, their
//! kinds, their rule hashes, and the forward edges as label pairs.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use cambium_index::sky::{Kind, SkyRef};
use cambium_index::{IndexRef, NodeKind};
use serde::{Deserialize, Serialize};

/// What differs between two indexes, by label. Empty means they agree.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GraphDiff {
    /// Live, known-kind nodes present only in the applied index.
    pub nodes_only_in_applied: Vec<String>,
    pub nodes_only_in_rebuilt: Vec<String>,
    /// `label -> (applied kind, rebuilt kind)` where the kinds disagree.
    pub kind_mismatch: Vec<(String, String, String)>,
    /// Rules whose hashes differ.
    pub rule_hash_mismatch: Vec<String>,
    /// `(from, to)` edges present only in one side.
    pub edges_only_in_applied: Vec<(String, String)>,
    pub edges_only_in_rebuilt: Vec<(String, String)>,
    /// Externals side tables that disagree, one line each: the repository
    /// input map, the per-repository path mapping and its `target_only`
    /// flags, the `.bzl` and package external-label rows. These decide how
    /// wide a diff goes before any query happens, and none of them is a node,
    /// an edge or a rule hash: an index that lost one describes the same
    /// graph and answers a different question. Absent from records written
    /// before the field existed, which read as agreeing here.
    #[serde(default)]
    pub externals_mismatch: Vec<String>,
    /// The Skyframe loading layer, node by node and edge by edge, where the
    /// applied index is *short*: a node or a reverse edge a cold build at the
    /// same commit has and the incrementally applied one does not.
    ///
    /// This is the table the dirty stage walks, so a short row means a change
    /// that should have reached a package does not, which is the one class of
    /// bug the design does not tolerate. It is also invisible to the node,
    /// edge and rule-hash comparisons above, because the two indexes describe
    /// the same target graph either way. Absent from records written before
    /// the field existed.
    #[serde(default)]
    pub sky_mismatch: Vec<String>,
    /// The other direction: layer rows the applied index has and a cold build
    /// does not. Counted and reported, and it does not fail the oracle.
    ///
    /// A refresh replaces the rows of the nodes its dump names and leaves
    /// every other row alone, which is what makes the incremental dump cheap;
    /// what it cannot do is *remove* a node, so a file, package or `.bzl` that
    /// stopped existing leaves its row behind. Such a row is inert (nothing
    /// depends on it, so no walk reaches a package through it) and it is an
    /// over-approximation of the same shape the rest of the engine is allowed:
    /// acceptable, and measured.
    #[serde(default)]
    pub sky_extra: Vec<String>,
    /// Totals, so a capped report still says how big the disagreement is.
    pub counts: DiffCounts,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffCounts {
    pub nodes_only_in_applied: usize,
    pub nodes_only_in_rebuilt: usize,
    pub kind_mismatch: usize,
    pub rule_hash_mismatch: usize,
    pub edges_only_in_applied: usize,
    pub edges_only_in_rebuilt: usize,
    #[serde(default)]
    pub externals_mismatch: usize,
    #[serde(default)]
    pub sky_mismatch: usize,
    #[serde(default)]
    pub sky_extra: usize,
}

impl GraphDiff {
    /// Whether the two indexes agree on everything a run's answer can depend
    /// on.
    ///
    /// `sky_extra` is deliberately not part of it: a layer row the applied
    /// index kept and a cold build never wrote costs bytes and nothing else.
    /// Everything else here is an answer that would differ.
    pub fn is_empty(&self) -> bool {
        self.holds_with_sky_missing(self.counts.sky_mismatch)
    }

    /// [`Self::is_empty`] with the loading layer's missing count replaced by
    /// `unexplained`: the rows left after the walk classified the ones a
    /// refresh could never have written (a directory created inside the
    /// window, a package that failed to load). See
    /// [`crate::walk::rebuild_rows`].
    pub fn holds_with_sky_missing(&self, unexplained: usize) -> bool {
        DiffCounts {
            sky_extra: 0,
            sky_mismatch: unexplained,
            ..self.counts.clone()
        } == DiffCounts::default()
    }

    pub fn summary(&self) -> String {
        let c = &self.counts;
        format!(
            "nodes only in applied: {}, only in rebuilt: {}; kind mismatches: {}; \
             rule hash mismatches: {}; edges only in applied: {}, only in rebuilt: {}; \
             externals mismatches: {}; loading layer rows missing: {} (extra: {})",
            c.nodes_only_in_applied,
            c.nodes_only_in_rebuilt,
            c.kind_mismatch,
            c.rule_hash_mismatch,
            c.edges_only_in_applied,
            c.edges_only_in_rebuilt,
            c.externals_mismatch,
            c.sky_mismatch,
            c.sky_extra
        )
    }
}

fn kind_name(k: NodeKind) -> &'static str {
    cambium_engine::inspect::kind_name(k)
}

/// A loading-layer node by name: `(kind, root, key)`, the identity the
/// comparison uses because two layers assign different ids.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SkyName {
    #[serde(with = "kind_name_serde")]
    pub kind: Kind,
    /// The root's name; empty for the workspace.
    pub root: String,
    pub key: String,
}

impl SkyName {
    /// Node `id` of a loading layer, by the identity that survives across
    /// lineages.
    pub(crate) fn of(v: SkyRef<'_>, id: u32) -> Self {
        SkyName {
            kind: v.kind_of(id),
            root: v.root_name(id).into_owned(),
            key: v.text(id).to_string(),
        }
    }
}

/// `KIND:[root]/[key]`, the spelling the capped string rows use.
impl fmt::Display for SkyName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:[{}]/[{}]", self.kind.name(), self.root, self.key)
    }
}

/// A [`Kind`] as its dump name, so a stored row reads as the dump does.
mod kind_name_serde {
    use cambium_index::sky::Kind;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(k: &Kind, s: S) -> Result<S::Ok, S::Error> {
        k.name().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Kind, D::Error> {
        let s = String::deserialize(d)?;
        Kind::from_dump(&s)
            .or(match s.as_str() {
                "IGNORED" => Some(Kind::Ignored),
                "TOMBSTONE" => Some(Kind::Tombstone),
                _ => None,
            })
            .ok_or_else(|| serde::de::Error::custom(format!("unknown loading-layer kind {s:?}")))
    }
}

/// One loading-layer row: a node, or a reverse edge between two.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "item", rename_all = "snake_case")]
pub enum SkyItem {
    Node(SkyName),
    Edge { from: SkyName, to: SkyName },
}

impl fmt::Display for SkyItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkyItem::Node(n) => write!(f, "{n}"),
            SkyItem::Edge { from, to } => write!(f, "{from} -> {to}"),
        }
    }
}

/// The loading-layer half of the diff, whole: every row, uncapped, so the
/// walk can classify each one rather than the first fifty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkyDiff {
    /// `Some((applied, rebuilt))` when the two sides disagree on whether a
    /// layer is present at all; the row lists are then empty.
    pub present: Option<(bool, bool)>,
    /// Rows the rebuilt layer has and the applied one does not, nodes first,
    /// each list in `(kind, root, key)` order.
    pub missing: Vec<SkyItem>,
    /// Rows the applied layer has and the rebuilt one does not.
    pub extra: Vec<SkyItem>,
}

/// The loading layer, node by node and edge by edge, restricted on both
/// sides to what a changed path in this repository can reach.
///
/// Comparing anything else would measure how much of the world each run's
/// server happened to load rather than what the index knows. A cold `//...`
/// build evaluates far more than an incremental run does: on the `bazel`
/// subject it leaves about five hundred `BZL_LOAD` nodes of other
/// repositories behind, some two thousand rows once their edges are counted,
/// and no main-repository diff can reach any of them.
pub fn sky_diff(applied: SkyRef<'_>, rebuilt: SkyRef<'_>) -> SkyDiff {
    let mut d = SkyDiff::default();
    if applied.present != rebuilt.present {
        d.present = Some((applied.present, rebuilt.present));
        return d;
    }
    if !applied.present {
        return d;
    }

    fn names(v: SkyRef<'_>) -> Vec<SkyName> {
        (0..v.len() as u32).map(|id| SkyName::of(v, id)).collect()
    }
    /// The nodes a changed path in this repository can reach: everything
    /// backwards from a file or directory state of the workspace itself.
    fn reachable(v: SkyRef<'_>) -> Vec<bool> {
        let mut seen = vec![false; v.len()];
        let mut stack: Vec<u32> = Vec::new();
        for id in 0..v.len() as u32 {
            let seed = matches!(v.kind_of(id), Kind::FileState | Kind::DirectoryListingState)
                && v.root[id as usize] == cambium_index::sky::WORKSPACE;
            if seed && !seen[id as usize] {
                seen[id as usize] = true;
                stack.push(id);
            }
        }
        while let Some(id) = stack.pop() {
            for &up in v.rdeps.row(id) {
                if !seen[up as usize] {
                    seen[up as usize] = true;
                    stack.push(up);
                }
            }
        }
        seen
    }
    let (na, nb) = (names(applied), names(rebuilt));
    let (ka, kb) = (reachable(applied), reachable(rebuilt));
    let kept = |names: &[SkyName], keep: &[bool]| -> BTreeSet<SkyName> {
        names
            .iter()
            .zip(keep)
            .filter(|(_, k)| **k)
            .map(|(n, _)| n.clone())
            .collect()
    };
    let (sa, sb) = (kept(&na, &ka), kept(&nb, &kb));
    for n in sa.difference(&sb) {
        d.extra.push(SkyItem::Node(n.clone()));
    }
    for n in sb.difference(&sa) {
        d.missing.push(SkyItem::Node(n.clone()));
    }

    fn edges<'a>(
        v: SkyRef<'_>,
        names: &'a [SkyName],
        keep: &[bool],
    ) -> BTreeSet<(&'a SkyName, &'a SkyName)> {
        let mut out = BTreeSet::new();
        for to in 0..v.len() as u32 {
            if !keep[to as usize] {
                continue;
            }
            for &from in v.rdeps.row(to) {
                if keep[from as usize] {
                    out.insert((&names[from as usize], &names[to as usize]));
                }
            }
        }
        out
    }
    let (ea, eb) = (edges(applied, &na, &ka), edges(rebuilt, &nb, &kb));
    for (from, to) in ea.difference(&eb) {
        d.extra.push(SkyItem::Edge {
            from: (*from).clone(),
            to: (*to).clone(),
        });
    }
    for (from, to) in eb.difference(&ea) {
        d.missing.push(SkyItem::Edge {
            from: (*from).clone(),
            to: (*to).clone(),
        });
    }
    d
}

/// Compares two indexes by label, ignoring ids, tombstones and unknown nodes.
/// Lists are capped at `cap` entries each; the counts are exact.
pub fn compare_by_label(applied: IndexRef<'_>, rebuilt: IndexRef<'_>, cap: usize) -> GraphDiff {
    compare_by_label_with_sky(applied, rebuilt, cap).0
}

/// [`compare_by_label`], also returning the loading-layer half whole: the
/// [`GraphDiff`] keeps `cap` rendered rows of it, the [`SkyDiff`] every row.
pub fn compare_by_label_with_sky(
    applied: IndexRef<'_>,
    rebuilt: IndexRef<'_>,
    cap: usize,
) -> (GraphDiff, SkyDiff) {
    fn live(v: IndexRef<'_>) -> BTreeMap<&str, u32> {
        (0..v.num_nodes())
            .filter(|&id| !v.is_tombstone(id) && v.kind_of(id) != NodeKind::Unknown)
            .map(|id| (v.label(id), id))
            .collect()
    }
    fn edges(v: IndexRef<'_>) -> BTreeSet<(&str, &str)> {
        let mut out = BTreeSet::new();
        for id in 0..v.num_nodes() {
            if v.is_tombstone(id) || v.kind_of(id) == NodeKind::Unknown {
                continue;
            }
            for &d in v.forward.row(id) {
                out.insert((v.label(id), v.label(d)));
            }
        }
        out
    }

    let (a, b) = (live(applied), live(rebuilt));
    let mut d = GraphDiff::default();
    let push = |v: &mut Vec<String>, n: &mut usize, s: String| {
        *n += 1;
        if v.len() < cap {
            v.push(s);
        }
    };
    for (label, &ia) in &a {
        match b.get(label) {
            None => push(
                &mut d.nodes_only_in_applied,
                &mut d.counts.nodes_only_in_applied,
                label.to_string(),
            ),
            Some(&ib) => {
                let (ka, kb) = (applied.kind_of(ia), rebuilt.kind_of(ib));
                if ka != kb {
                    d.counts.kind_mismatch += 1;
                    if d.kind_mismatch.len() < cap {
                        d.kind_mismatch.push((
                            label.to_string(),
                            kind_name(ka).into(),
                            kind_name(kb).into(),
                        ));
                    }
                } else if ka == NodeKind::Rule
                    && applied.hash[ia as usize] != rebuilt.hash[ib as usize]
                {
                    push(
                        &mut d.rule_hash_mismatch,
                        &mut d.counts.rule_hash_mismatch,
                        label.to_string(),
                    );
                }
            }
        }
    }
    for label in b.keys() {
        if !a.contains_key(label) {
            push(
                &mut d.nodes_only_in_rebuilt,
                &mut d.counts.nodes_only_in_rebuilt,
                label.to_string(),
            );
        }
    }
    let (ea, eb) = (edges(applied), edges(rebuilt));
    for e in ea.difference(&eb) {
        d.counts.edges_only_in_applied += 1;
        if d.edges_only_in_applied.len() < cap {
            d.edges_only_in_applied
                .push((e.0.to_string(), e.1.to_string()));
        }
    }
    for e in eb.difference(&ea) {
        d.counts.edges_only_in_rebuilt += 1;
        if d.edges_only_in_rebuilt.len() < cap {
            d.edges_only_in_rebuilt
                .push((e.0.to_string(), e.1.to_string()));
        }
    }
    compare_externals(applied, rebuilt, cap, &mut d);
    let sky = sky_diff(applied.sky, rebuilt.sky);
    compare_sky(&sky, cap, &mut d);
    (d, sky)
}

/// The Skyframe loading layer, rendered into the capped string rows.
///
/// A splice refreshes only the packages the run re-queried, and a cold build
/// at the same commit dumps the whole repository; the two must describe the
/// same loading-phase graph. Nothing above sees a disagreement here (the
/// layer is neither a target nor an edge between targets) but it is what the
/// dirty stage walks, so an index whose layer is short of an edge answers a
/// smaller question than a cold build would for every commit after it.
///
/// Node identity is `(kind, root name, key)`, so the comparison survives the
/// two layers assigning different ids, exactly as the rule-layer comparison
/// survives two lineages assigning different node ids. The rows come from
/// [`sky_diff`]; this only spells them.
fn compare_sky(sky: &SkyDiff, cap: usize, d: &mut GraphDiff) {
    let mut missing = |s: String| {
        d.counts.sky_mismatch += 1;
        if d.sky_mismatch.len() < cap {
            d.sky_mismatch.push(s);
        }
    };
    if let Some((a, b)) = sky.present {
        missing(format!("loading layer present: applied {a}, rebuilt {b}"));
        return;
    }
    let mut extra = |s: String| {
        d.counts.sky_extra += 1;
        if d.sky_extra.len() < cap {
            d.sky_extra.push(s);
        }
    };
    for item in &sky.extra {
        extra(only_in(item, "applied"));
    }
    for item in &sky.missing {
        missing(only_in(item, "rebuilt"));
    }
}

/// `node only in <side>: <name>` or `edge only in <side>: <from> -> <to>`.
fn only_in(item: &SkyItem, side: &str) -> String {
    match item {
        SkyItem::Node(n) => format!("node only in {side}: {n}"),
        SkyItem::Edge { from, to } => format!("edge only in {side}: {from} -> {to}"),
    }
}

/// The externals side tables, keyed by name rather than by id.
///
/// Every one of these is derived from the same walk over the same workspace
/// `.bzl` files as the load closure, carried through a splice rather than
/// recomputed, and read by [`cambium_engine::externals`] *before* anything is
/// queried. A column a splice drops therefore changes how wide a diff goes
/// while leaving the graph itself byte-identical, which is exactly the shape
/// the node/edge/hash comparison above cannot see.
fn compare_externals(applied: IndexRef<'_>, rebuilt: IndexRef<'_>, cap: usize, d: &mut GraphDiff) {
    let mut note = |s: String| {
        d.counts.externals_mismatch += 1;
        if d.externals_mismatch.len() < cap {
            d.externals_mismatch.push(s);
        }
    };
    if applied.bzl_closure != rebuilt.bzl_closure {
        note(format!(
            "bzl closure present: applied {}, rebuilt {}",
            applied.bzl_closure, rebuilt.bzl_closure
        ));
        return;
    }
    if !applied.bzl_closure {
        return;
    }

    // Repositories: canonical name and path mapping, by apparent name.
    fn repos(v: IndexRef<'_>) -> BTreeMap<&str, (&str, Vec<String>)> {
        (0..v.num_repos())
            .map(|r| {
                let maps = v
                    .repo_path_maps(r)
                    .into_iter()
                    .map(|m| format!("{} -> {}", m.from, m.to))
                    .collect();
                (v.repo_name(r), (v.repo_canonical.get(r), maps))
            })
            .collect()
    }
    let (ra, rb) = (repos(applied), repos(rebuilt));
    for (name, a) in &ra {
        match rb.get(name) {
            None => note(format!("repository `{name}` only in applied")),
            Some(b) if a != b => note(format!("repository `{name}`: applied {a:?}, rebuilt {b:?}")),
            Some(_) => {}
        }
    }
    for name in rb.keys() {
        if !ra.contains_key(name) {
            note(format!("repository `{name}` only in rebuilt"));
        }
    }

    // Input patterns, with their `target_only` flag and the repositories they
    // feed, keyed by the pattern's own text.
    fn patterns(v: IndexRef<'_>) -> BTreeMap<String, (bool, Vec<&str>)> {
        (0..v.repo_input_patterns.len())
            .map(|i| {
                let p = i as u32;
                let key = format!(
                    "{}{}",
                    v.repo_input_patterns.get(p),
                    if v.repo_input_prefix[i] { "**" } else { "" }
                );
                let repos = v
                    .repo_inputs
                    .row(p)
                    .iter()
                    .map(|&r| v.repo_name(r))
                    .collect();
                (key, (v.repo_input_target_only[i], repos))
            })
            .collect()
    }
    let (pa, pb) = (patterns(applied), patterns(rebuilt));
    for (key, a) in &pa {
        match pb.get(key) {
            None => note(format!("input pattern `{key}` only in applied")),
            Some(b) if a != b => note(format!(
                "input pattern `{key}`: applied {a:?}, rebuilt {b:?}"
            )),
            Some(_) => {}
        }
    }
    for key in pb.keys() {
        if !pa.contains_key(key) {
            note(format!("input pattern `{key}` only in rebuilt"));
        }
    }

    // The `.bzl` rows: which repositories a workspace-closure file defines,
    // and the external labels it and each BUILD file load.
    fn bzl_rows(v: IndexRef<'_>) -> BTreeMap<&str, (Vec<&str>, Vec<&str>)> {
        (0..v.num_bzl())
            .map(|id| {
                let repos = v.bzl_repos_of(id).iter().map(|&r| v.repo_name(r)).collect();
                (v.bzl_path(id), (repos, v.bzl_external(id)))
            })
            .collect()
    }
    let (ba, bb) = (bzl_rows(applied), bzl_rows(rebuilt));
    for (path, a) in &ba {
        if let Some(b) = bb.get(path) {
            if a != b {
                note(format!("`{path}`: applied {a:?}, rebuilt {b:?}"));
            }
        }
    }

    fn pkg_ext(v: IndexRef<'_>) -> BTreeMap<&str, Vec<&str>> {
        (0..v.num_packages())
            .map(|p| (v.package_name(p), v.package_external(p)))
            .filter(|(_, e)| !e.is_empty())
            .collect()
    }
    let (ka, kb) = (pkg_ext(applied), pkg_ext(rebuilt));
    for (pkg, a) in &ka {
        match kb.get(pkg) {
            None => note(format!("external loads of `{pkg}` only in applied")),
            Some(b) if a != b => note(format!(
                "external loads of `{pkg}`: applied {a:?}, rebuilt {b:?}"
            )),
            Some(_) => {}
        }
    }
    for pkg in kb.keys() {
        if !ka.contains_key(pkg) {
            note(format!("external loads of `{pkg}` only in rebuilt"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cambium_index::{IndexBuilder, PackageUpdate, TargetRow};

    fn index(extra: bool, hash: u8) -> cambium_index::IndexData {
        let mut b = IndexBuilder::new(cambium_index::new_lineage());
        let mut core = TargetRow::rule("//lib:core", "cc_library", vec!["//lib:a.cc".into()]);
        core.rule_hash = [hash; 32];
        let mut targets = vec![core, TargetRow::source_file("//lib:a.cc")];
        if extra {
            targets.push(TargetRow::rule(
                "//lib:extra",
                "cc_library",
                vec!["//lib:core".into()],
            ));
        }
        b.apply_packages(&[PackageUpdate {
            package: "lib".into(),
            targets,
            ..Default::default()
        }]);
        b.finish("c")
    }

    #[test]
    fn identical_graphs_have_an_empty_diff() {
        let a = index(true, 1);
        let b = index(true, 1);
        assert!(compare_by_label(a.view(), b.view(), 10).is_empty());
    }

    #[test]
    fn a_missing_node_and_a_moved_hash_are_reported() {
        let a = index(true, 1);
        let b = index(false, 2);
        let d = compare_by_label(a.view(), b.view(), 10);
        assert_eq!(d.nodes_only_in_applied, vec!["//lib:extra"]);
        assert_eq!(d.rule_hash_mismatch, vec!["//lib:core"]);
        assert_eq!(
            d.edges_only_in_applied,
            vec![("//lib:extra".to_string(), "//lib:core".to_string())]
        );
        assert!(!d.is_empty());
        assert!(d.summary().contains("rule hash mismatches: 1"));
    }

    /// The regression this half of the oracle exists for: two indexes with
    /// identical nodes, edges and rule hashes, one of which lost a
    /// repository's path mapping on the way through a splice. Before this,
    /// the diff was empty and the run went on to re-query eleven hundred
    /// packages.
    #[test]
    fn a_dropped_path_mapping_is_reported_though_the_graph_is_identical() {
        fn index(mapped: bool) -> cambium_index::IndexData {
            let mut b = IndexBuilder::new(cambium_index::new_lineage());
            b.apply_packages(&[PackageUpdate {
                package: "lib".into(),
                targets: vec![TargetRow::rule("//lib:core", "cc_library", vec![])],
                ..Default::default()
            }]);
            let mut c = cambium_index::BzlClosure::default();
            c.inputs.repos.insert("mod".into());
            if mapped {
                c.inputs.path_maps.entry("mod".into()).or_default().insert(
                    cambium_index::externals::PathMap::exhaustive_dir("api/".into()),
                );
            }
            b.set_bzl_closure(c);
            b.finish("c")
        }
        let (a, b) = (index(false), index(true));
        let d = compare_by_label(a.view(), b.view(), 10);
        assert_eq!(d.counts.nodes_only_in_applied, 0);
        assert_eq!(d.counts.rule_hash_mismatch, 0);
        assert_eq!(d.counts.edges_only_in_applied, 0);
        assert_eq!(d.counts.externals_mismatch, 1, "{:?}", d.externals_mismatch);
        assert!(d.externals_mismatch[0].contains("api/"), "{d:?}");
        assert!(!d.is_empty());
    }

    /// The same regression one layer down: two indexes whose nodes, edges,
    /// rule hashes and externals tables agree exactly, one of whose loading
    /// layer is short of the edge a new sub-package travels. The dirty stage
    /// walks that layer, so the short one answers a narrower question for
    /// every commit after it, and nothing else in the oracle can see it.
    #[test]
    fn a_short_loading_layer_is_reported_though_the_graph_is_identical() {
        const FULL: &str = "\
PACKAGE:lib
  Group 1:
    PACKAGE_LOOKUP:lib

  Group 2:
    PACKAGE_LOOKUP:lib/sub

PACKAGE_LOOKUP:lib/sub
  Group 1:
    FILE:[/ws]/[lib/sub/BUILD]

FILE:[/ws]/[lib/sub/BUILD]
  Group 1:
    FILE_STATE:[/ws]/[lib/sub/BUILD]
";
        // The same dump with the package's lookup of its sub-directory gone.
        const SHORT: &str = "\
PACKAGE:lib
  Group 1:
    PACKAGE_LOOKUP:lib

PACKAGE_LOOKUP:lib/sub
  Group 1:
    FILE:[/ws]/[lib/sub/BUILD]

FILE:[/ws]/[lib/sub/BUILD]
  Group 1:
    FILE_STATE:[/ws]/[lib/sub/BUILD]
";
        fn index(dump: &str) -> cambium_index::IndexData {
            let mut b = IndexBuilder::new(cambium_index::new_lineage());
            b.apply_packages(&[PackageUpdate {
                package: "lib".into(),
                targets: vec![TargetRow::rule("//lib:core", "cc_library", vec![])],
                ..Default::default()
            }]);
            let mut l = cambium_index::SkyLayer::new();
            l.apply_dump(dump, &cambium_index::sky::Paths::new("/ws", "/ob"));
            b.set_sky_layer(l);
            b.finish("c")
        }
        // The short one as the *applied* index: a row the cold build has and
        // it does not, which is a change that would not reach a package.
        let (a, b) = (index(SHORT), index(FULL));
        let d = compare_by_label(a.view(), b.view(), 10);
        assert_eq!(d.counts.nodes_only_in_applied, 0);
        assert_eq!(d.counts.rule_hash_mismatch, 0);
        assert_eq!(d.counts.edges_only_in_applied, 0);
        assert_eq!(d.counts.externals_mismatch, 0);
        // Two rows: the edge the short layer lacks, and the package node
        // itself, which without that edge nothing can reach any more.
        assert_eq!(d.counts.sky_mismatch, 2, "{:?}", d.sky_mismatch);
        assert!(
            d.sky_mismatch
                .iter()
                .any(|l| l.contains("PACKAGE_LOOKUP") && l.contains("only in rebuilt")),
            "{d:?}"
        );
        assert!(!d.is_empty());
        assert!(d.summary().contains("loading layer rows missing: 2"));

        // The structured half carries every row where the string lists are
        // capped, and spells each one exactly as the string row does.
        let (capped, sky) = compare_by_label_with_sky(a.view(), b.view(), 1);
        assert_eq!(capped.sky_mismatch.len(), 1, "{capped:?}");
        assert_eq!(capped.counts.sky_mismatch, 2);
        assert!(sky.present.is_none());
        assert_eq!(sky.missing.len(), 2, "{sky:?}");
        assert!(sky.extra.is_empty(), "{sky:?}");
        let node = sky
            .missing
            .iter()
            .find_map(|i| match i {
                SkyItem::Node(n) => Some(n),
                _ => None,
            })
            .expect("a node row");
        assert_eq!(
            (node.kind, node.root.as_str(), node.key.as_str()),
            (Kind::Package, "", "lib")
        );
        let (from, to) = sky
            .missing
            .iter()
            .find_map(|i| match i {
                SkyItem::Edge { from, to } => Some((from, to)),
                _ => None,
            })
            .expect("an edge row");
        assert_eq!((from.kind, from.key.as_str()), (Kind::Package, "lib"));
        assert_eq!((to.kind, to.key.as_str()), (Kind::PackageLookup, "lib/sub"));
        for (item, row) in sky.missing.iter().zip(&d.sky_mismatch) {
            assert!(row.ends_with(&item.to_string()), "{row} vs {item}");
        }
        // And a row round-trips through its stored shape, kind by name.
        let text = serde_json::to_string(&sky.missing).unwrap();
        assert!(
            text.contains("\"item\":\"edge\"") && text.contains("\"kind\":\"PACKAGE_LOOKUP\""),
            "{text}"
        );
        let back: Vec<SkyItem> = serde_json::from_str(&text).unwrap();
        assert_eq!(back, sky.missing);

        // The other way round is an over-approximation: a row the applied
        // index kept and a cold build never wrote is inert, so it is counted
        // and does not fail the oracle. A refresh replaces the rows its dump
        // names and cannot remove a node, so this is what a long walk leaves.
        let d = compare_by_label(b.view(), a.view(), 10);
        assert_eq!(d.counts.sky_mismatch, 0, "{:?}", d.sky_mismatch);
        assert_eq!(d.counts.sky_extra, 2, "{:?}", d.sky_extra);
        assert!(d.sky_extra[0].contains("only in applied"), "{d:?}");
        assert!(d.is_empty(), "{d:?}");
        assert!(d.holds_with_sky_missing(0));
        assert!(!d.holds_with_sky_missing(1));

        // And an index with no layer at all is told apart from one whose
        // loading phase touched nothing.
        let none = {
            let mut b = IndexBuilder::new(cambium_index::new_lineage());
            b.apply_packages(&[PackageUpdate {
                package: "lib".into(),
                targets: vec![TargetRow::rule("//lib:core", "cc_library", vec![])],
                ..Default::default()
            }]);
            b.finish("c")
        };
        let (d, sky) = compare_by_label_with_sky(none.view(), b.view(), 10);
        assert_eq!(d.counts.sky_mismatch, 1, "{:?}", d.sky_mismatch);
        assert!(d.sky_mismatch[0].contains("present"), "{d:?}");
        assert_eq!(sky.present, Some((false, true)));
        assert!(sky.missing.is_empty() && sky.extra.is_empty());
    }

    #[test]
    fn a_diff_written_before_externals_mismatch_existed_still_reads() {
        let old = r#"{"nodes_only_in_applied":[],"nodes_only_in_rebuilt":[],"kind_mismatch":[],
            "rule_hash_mismatch":[],"edges_only_in_applied":[],"edges_only_in_rebuilt":[],
            "counts":{"nodes_only_in_applied":0,"nodes_only_in_rebuilt":0,"kind_mismatch":0,
            "rule_hash_mismatch":0,"edges_only_in_applied":0,"edges_only_in_rebuilt":0}}"#;
        let d: GraphDiff = serde_json::from_str(old).expect("pre-field record parses");
        assert!(d.externals_mismatch.is_empty());
        assert_eq!(d.counts.externals_mismatch, 0);
    }
}
