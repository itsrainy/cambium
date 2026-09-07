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
}

impl GraphDiff {
    pub fn is_empty(&self) -> bool {
        self.counts == DiffCounts::default()
    }

    pub fn summary(&self) -> String {
        let c = &self.counts;
        format!(
            "nodes only in applied: {}, only in rebuilt: {}; kind mismatches: {}; \
             rule hash mismatches: {}; edges only in applied: {}, only in rebuilt: {}",
            c.nodes_only_in_applied,
            c.nodes_only_in_rebuilt,
            c.kind_mismatch,
            c.rule_hash_mismatch,
            c.edges_only_in_applied,
            c.edges_only_in_rebuilt
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
    d
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
}
