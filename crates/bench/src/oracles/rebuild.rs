//! The incremental-equals-full oracle on real input.
//!
//! After a history has brought an index forward commit by commit, a cold build
//! at the same commit must describe the same graph. Ids are not comparable
//! across lineages, so the comparison is by label: the live nodes, their
//! kinds, their rule hashes, and the forward edges as label change sets.

use std::collections::{BTreeMap, BTreeSet};

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
    /// an edge or a rule hash — an index that lost one describes the same
    /// graph and answers a different question. Absent from records written
    /// before the field existed; `bench report` reads every history file.
    #[serde(default)]
    pub externals_mismatch: Vec<String>,
    /// The Skyframe loading layer, node by node and edge by edge: layer nodes
    /// present on one side only, and reverse edges present on one side only,
    /// each as one line. This is the table the dirty stage walks, so an index
    /// that carried a stale or short one would answer a *narrower* question
    /// than a cold build at the same commit while describing the same graph —
    /// which is precisely what the node, edge and rule-hash comparisons above
    /// cannot see. Absent from records written before the field existed.
    #[serde(default)]
    pub sky_mismatch: Vec<String>,
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
}

impl GraphDiff {
    pub fn is_empty(&self) -> bool {
        self.counts == DiffCounts::default()
    }

    pub fn summary(&self) -> String {
        let c = &self.counts;
        format!(
            "nodes only in applied: {}, only in rebuilt: {}; kind mismatches: {}; \
             rule hash mismatches: {}; edges only in applied: {}, only in rebuilt: {}; \
             externals mismatches: {}; loading layer mismatches: {}",
            c.nodes_only_in_applied,
            c.nodes_only_in_rebuilt,
            c.kind_mismatch,
            c.rule_hash_mismatch,
            c.edges_only_in_applied,
            c.edges_only_in_rebuilt,
            c.externals_mismatch,
            c.sky_mismatch
        )
    }
}

fn kind_name(k: NodeKind) -> &'static str {
    cambium_engine::inspect::kind_name(k)
}

/// Compares two indexes by label, ignoring ids, tombstones and unknown nodes.
/// Lists are capped at `cap` entries each; the counts are exact.
pub fn compare_by_label(applied: IndexRef<'_>, rebuilt: IndexRef<'_>, cap: usize) -> GraphDiff {
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
    compare_sky(applied, rebuilt, cap, &mut d);
    d
}

/// The Skyframe loading layer, node by node and edge by edge.
///
/// A splice refreshes only the packages the run re-queried, and a cold build
/// at the same commit dumps the whole repository; the two must describe the
/// same loading-phase graph. Nothing above sees a disagreement here — the
/// layer is neither a target nor an edge between targets — but it is what the
/// dirty stage walks, so an index whose layer is short of an edge answers a
/// smaller question than a cold build would for every commit after it.
///
/// Node identity is `(kind, root name, key)`, so the comparison survives the
/// two layers assigning different ids, exactly as the rule-layer comparison
/// survives two lineages assigning different node ids.
fn compare_sky(applied: IndexRef<'_>, rebuilt: IndexRef<'_>, cap: usize, d: &mut GraphDiff) {
    let mut note = |s: String| {
        d.counts.sky_mismatch += 1;
        if d.sky_mismatch.len() < cap {
            d.sky_mismatch.push(s);
        }
    };
    if applied.sky.present != rebuilt.sky.present {
        note(format!(
            "loading layer present: applied {}, rebuilt {}",
            applied.sky.present, rebuilt.sky.present
        ));
        return;
    }
    if !applied.sky.present {
        return;
    }

    /// `(kind, root, key)` for every node, and the same for each of its
    /// reverse edges' other end.
    fn names(v: cambium_index::sky::SkyRef<'_>) -> Vec<String> {
        (0..v.len() as u32)
            .map(|id| {
                format!(
                    "{}:[{}]/[{}]",
                    v.kind_of(id).name(),
                    v.roots.get(v.root[id as usize]),
                    v.text(id)
                )
            })
            .collect()
    }
    let (na, nb) = (names(applied.sky), names(rebuilt.sky));
    let (sa, sb): (BTreeSet<&str>, BTreeSet<&str>) = (
        na.iter().map(String::as_str).collect(),
        nb.iter().map(String::as_str).collect(),
    );
    for n in sa.difference(&sb) {
        note(format!("node only in applied: {n}"));
    }
    for n in sb.difference(&sa) {
        note(format!("node only in rebuilt: {n}"));
    }

    fn edges<'a>(v: cambium_index::sky::SkyRef<'_>, names: &'a [String]) -> BTreeSet<(&'a str, &'a str)> {
        let mut out = BTreeSet::new();
        for to in 0..v.len() as u32 {
            for &from in v.rdeps.row(to) {
                out.insert((names[from as usize].as_str(), names[to as usize].as_str()));
            }
        }
        out
    }
    let (ea, eb) = (edges(applied.sky, &na), edges(rebuilt.sky, &nb));
    for (from, to) in ea.difference(&eb) {
        note(format!("edge only in applied: {from} -> {to}"));
    }
    for (from, to) in eb.difference(&ea) {
        note(format!("edge only in rebuilt: {from} -> {to}"));
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
        let (a, b) = (index(FULL), index(SHORT));
        let d = compare_by_label(a.view(), b.view(), 10);
        assert_eq!(d.counts.nodes_only_in_applied, 0);
        assert_eq!(d.counts.rule_hash_mismatch, 0);
        assert_eq!(d.counts.edges_only_in_applied, 0);
        assert_eq!(d.counts.externals_mismatch, 0);
        assert_eq!(d.counts.sky_mismatch, 1, "{:?}", d.sky_mismatch);
        assert!(
            d.sky_mismatch[0].contains("PACKAGE_LOOKUP")
                && d.sky_mismatch[0].contains("only in applied"),
            "{d:?}"
        );
        assert!(!d.is_empty());
        assert!(d.summary().contains("loading layer mismatches: 1"));

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
        let d = compare_by_label(a.view(), none.view(), 10);
        assert_eq!(d.counts.sky_mismatch, 1, "{:?}", d.sky_mismatch);
        assert!(d.sky_mismatch[0].contains("present"), "{d:?}");
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
