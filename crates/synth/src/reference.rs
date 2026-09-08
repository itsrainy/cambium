//! The naive O(graph) reference algorithm (spec 12.4).
//!
//! For every target, hash its own definition together with the transitive
//! closure of its inputs — the thing the whole incremental design exists to
//! avoid computing. Diffing those hashes between two revisions gives the ground
//! truth the engine's answer is checked against.

use std::collections::{BTreeMap, BTreeSet};

use crate::graph::SynthGraph;

/// `label -> deep hash` for every live target.
pub fn deep_hashes(g: &SynthGraph) -> BTreeMap<String, [u8; 32]> {
    let n = g.targets.len();
    let mut memo: Vec<Option<[u8; 32]>> = vec![None; n];

    // Deps always point to strictly lower layers, so computing in layer order
    // means every dependency is memoised before it is needed.
    let mut order: Vec<u32> = (0..n as u32)
        .filter(|&i| g.targets[i as usize].alive)
        .collect();
    order.sort_by_key(|&i| g.targets[i as usize].layer);

    let mut out = BTreeMap::new();
    for id in order {
        let t = &g.targets[id as usize];
        let mut h = blake3::Hasher::new();
        // The target's own definition: label, class, attributes, tags, the
        // labels of its deps and srcs.
        h.update(&g.target_hash(id));
        // The rule implementation, which lives in the package's .bzl closure.
        h.update(&g.bzl_closure_hash(t.package));
        // File contents.
        for (i, &slot) in t.srcs.iter().enumerate() {
            h.update(g.src_label(id, slot).as_bytes());
            h.update(&t.src_versions[i].to_le_bytes());
        }
        for &(_, ft, fs) in &t.foreign_srcs {
            h.update(g.src_label(ft, fs).as_bytes());
            h.update(&g.src_version(ft, fs).to_le_bytes());
        }
        // Everything the deps transitively depend on.
        for &d in &t.deps {
            if !g.targets[d as usize].alive {
                continue;
            }
            match memo[d as usize] {
                Some(dh) => h.update(&dh),
                // Only possible if a mutation broke the layering invariant.
                None => h.update(&g.target_hash(d)),
            };
        }
        let digest = *h.finalize().as_bytes();
        memo[id as usize] = Some(digest);
        out.insert(g.target_label(id), digest);
    }
    out
}

/// Targets in `after` whose deep hash differs from `before`, i.e. every target
/// whose output could have changed. New targets count; deleted ones do not
/// exist to be built.
pub fn reference_affected(before: &SynthGraph, after: &SynthGraph) -> BTreeSet<String> {
    let b = deep_hashes(before);
    let a = deep_hashes(after);
    a.into_iter()
        .filter(|(label, hash)| b.get(label) != Some(hash))
        .map(|(label, _)| label)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutate::{apply_one, Mutation};
    use crate::spec::GraphSpec;

    fn graph() -> SynthGraph {
        crate::generate(&GraphSpec::preset("small").unwrap())
    }

    #[test]
    fn an_unchanged_graph_affects_nothing() {
        let g = graph();
        assert!(reference_affected(&g, &g.clone()).is_empty());
    }

    #[test]
    fn a_modified_source_affects_its_consumers_transitively() {
        let before = graph();
        let mut after = before.clone();
        let t = (0..after.targets.len() as u32)
            .find(|&i| !after.targets[i as usize].srcs.is_empty())
            .unwrap();
        let slot = after.targets[t as usize].srcs[0];
        apply_one(&mut after, &Mutation::ModifySource { target: t, slot });

        let affected = reference_affected(&before, &after);
        assert!(affected.contains(&after.target_label(t)));
        // Everyone that depends on it, directly or not.
        for id in 0..after.targets.len() as u32 {
            if after.targets[id as usize].deps.contains(&t) {
                assert!(
                    affected.contains(&after.target_label(id)),
                    "{} should be affected",
                    after.target_label(id)
                );
            }
        }
    }

    #[test]
    fn a_bzl_edit_affects_every_loading_package() {
        let before = graph();
        let mut after = before.clone();
        let bzl = 0u32;
        let loaders: Vec<u32> = after.bzl[bzl as usize].loaded_by.clone();
        apply_one(&mut after, &Mutation::EditBzl { bzl });
        let affected = reference_affected(&before, &after);
        for p in loaders {
            for &t in &after.packages[p as usize].targets {
                if after.targets[t as usize].alive {
                    assert!(affected.contains(&after.target_label(t)));
                }
            }
        }
    }

    #[test]
    fn deleting_a_target_affects_its_consumers() {
        let before = graph();
        let mut after = before.clone();
        let victim = (0..after.targets.len() as u32)
            .find(|&i| after.targets.iter().any(|t| t.alive && t.deps.contains(&i)))
            .unwrap();
        apply_one(&mut after, &Mutation::DeleteTarget { target: victim });
        let affected = reference_affected(&before, &after);
        assert!(!affected.contains(&after.target_label(victim)));
        for id in 0..after.targets.len() as u32 {
            let t = &after.targets[id as usize];
            if t.alive && t.deps.contains(&victim) {
                assert!(affected.contains(&after.target_label(id)));
            }
        }
    }
}
