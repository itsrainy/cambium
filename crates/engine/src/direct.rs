//! The direct set (spec 6.7) and what is derived from it: the reverse walk's
//! seeds, the fixpoint over repositories the walk itself dirties, and the
//! `--explain` paths back to them.
//!
//! Everything here is generic over [`GraphView`], so a materialised head index
//! and a delta overlay go through one implementation and the two paths cannot
//! disagree about anything after the splice.

use std::collections::{BTreeMap, BTreeSet};

use cambium_index::{walk, GraphView, IndexRef, SpliceReport, WalkOptions};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::report::{AffectedTarget, DirectReason, Explanation};

/// Everything the direct set is derived from, once the splice has happened.
pub(crate) struct DirectInputs<'a> {
    pub(crate) splice: &'a SpliceReport,
    pub(crate) deleted_by_package: &'a [(String, Vec<u32>)],
    pub(crate) changed_file_ids: &'a [(u32, String)],
    pub(crate) bzl_dirty: &'a BTreeSet<String>,
    pub(crate) external_nodes: &'a [(u32, String)],
    /// Starlark build settings a `.bazelrc` edit changed, as `(id, label)`.
    pub(crate) flag_nodes: &'a [(u32, String)],
    /// Main-repo rule targets that external repositories' generated targets
    /// name, when the base index records any. `None` disables the fixpoint.
    pub(crate) label_inputs: Option<&'a LabelInputs<'a>>,
}

/// The main-repo *targets* external repositories are built around.
///
/// The index's pattern table records these as
/// [`cambium_index::externals::Pattern::Label`]: a repository rule wrote a
/// BUILD file naming `@envoy//changelogs`, so `@envoy_repo`'s generated
/// targets depend on `//changelogs:changelogs` — a `filegroup` over a glob
/// whose members only the graph knows. Resolving the labels to node ids once
/// per run turns "did the change reach that target?" into a lookup on the
/// affected set.
pub(crate) struct LabelInputs<'a> {
    base: IndexRef<'a>,
    /// Node id -> the apparent names of the repositories built around it.
    by_node: FxHashMap<u32, Vec<String>>,
    /// Repositories the diff already dirtied, which need no second look.
    already: BTreeSet<String>,
}

/// Resolves the base index's label patterns to node ids, or `None` when there
/// are none — which is every index written before this table had them, and
/// every repository whose rules name no main-repo target.
pub(crate) fn label_inputs(
    base: IndexRef<'_>,
    already: BTreeSet<String>,
) -> Option<LabelInputs<'_>> {
    let rows = base.label_inputs();
    if rows.is_empty() {
        return None;
    }
    let mut by_node: FxHashMap<u32, Vec<String>> = FxHashMap::default();
    for (label, repos) in rows {
        // A label naming no node in this index names nothing that can change.
        let Some(id) = base.id_of(label) else {
            continue;
        };
        let e: &mut Vec<String> = by_node.entry(id).or_default();
        // `feeds` is not consulted: only rule (1) ever applies here. A
        // repository rule cannot read a rule target's *output* — it runs
        // during loading, before anything is built — so whatever moved this
        // target cannot have moved a `.bzl` the repository exports, whether or
        // not some other path also feeds the rule.
        for (r, _feeds) in repos {
            let n = base.repo_name(r).to_string();
            if !e.contains(&n) {
                e.push(n);
            }
        }
    }
    (!by_node.is_empty()).then_some(LabelInputs {
        base,
        by_node,
        already,
    })
}

/// Closes the answer over the repositories the walk itself dirtied.
///
/// A repository whose generated targets name a main-repo rule target moves
/// when that target moves — envoy's `bazel/repo.bzl` writes a BUILD file whose
/// `genrule` lists `@envoy//changelogs` in `tools`, so a new
/// `changelogs/current/x.rst` moves `@envoy_repo`'s targets. Which files that
/// `filegroup` globs is not knowable statically, but it is knowable from the
/// graph: the filegroup node has edges to its sources, so the reverse walk
/// from the changed file reaches `//changelogs:changelogs`, and this reads the
/// answer off the affected set.
///
/// The repositories found that way are seeded by **rule (1) alone**: their
/// leaf nodes go into the direct set, and the walk runs again. Rule (2) never
/// applies. A repository rule runs during loading and cannot read a rule
/// target's *output* — nothing is built yet — so a rule target it names can
/// only be a dependency of what the rule *writes*, and nothing the rule wrote,
/// no `.bzl` it exports, can have moved. That is the same argument that makes
/// these inputs `target_only` in the index, arrived at from the other side.
///
/// Seeding those leaves can make more targets affected, which can reach more
/// such labels, so this is a fixpoint. It terminates: every iteration either
/// finds a repository not yet seen or stops, and there are finitely many
/// repositories. Returns how many repositories it dirtied.
pub(crate) fn close_over_repository_targets<V: GraphView + ?Sized>(
    view: &V,
    li: &LabelInputs<'_>,
    direct: &mut Vec<u32>,
    reasons: &mut FxHashMap<u32, DirectReason>,
    result: &mut walk::WalkResult,
    wopts: &WalkOptions,
) -> usize {
    let mut seen = li.already.clone();
    let mut in_direct: FxHashSet<u32> = direct.iter().copied().collect();
    let mut found = 0usize;
    let mut scratch: Vec<u32> = Vec::new();
    loop {
        // Which repositories does the answer so far name, that the diff did
        // not already dirty? `via` is the target that named each, for
        // `--explain`.
        let mut hit: BTreeMap<String, String> = BTreeMap::new();
        for &id in &result.affected {
            let Some(repos) = li.by_node.get(&id) else {
                continue;
            };
            for r in repos {
                if seen.contains(r) {
                    continue;
                }
                hit.entry(r.clone())
                    .or_insert_with(|| view.label(id).to_string());
            }
        }
        if hit.is_empty() {
            return found;
        }
        // One target naming a large share of every repository is the
        // signature of a label the index attributed to the fallback universe
        // rather than to the repositories that read it: on a 900k-target
        // repository, `//cloud/common/dbfx:go_default_library` seeded 3,819
        // repositories at once and the walk took the graph. The answer is
        // still sound, and this is where it should be visible.
        {
            let mut by_via: BTreeMap<&str, usize> = BTreeMap::new();
            for via in hit.values() {
                *by_via.entry(via.as_str()).or_default() += 1;
            }
            for (via, n) in by_via {
                if n >= 8 {
                    tracing::info!(
                        via,
                        repositories = n,
                        "one target names many external repositories; if the label was \
                         attributed to the fallback universe, every consumer of every \
                         repository goes direct (cambi index externals shows the row)"
                    );
                }
            }
        }
        let mut added = false;
        for (repo, via) in hit {
            seen.insert(repo.clone());
            found += 1;
            tracing::debug!(
                %repo, %via,
                "an external repository is built around a target the change reached"
            );
            let one: BTreeSet<String> = [repo.clone()].into_iter().collect();
            // Rule (1) only: the empty `load_repos` is what says so.
            let r = crate::externals::reach(li.base, &one, &BTreeSet::new());
            for id in r.nodes {
                let consumers: Vec<u32> = view.reverse_row(id, &mut scratch).to_vec();
                for c in consumers {
                    if in_direct.insert(c) {
                        direct.push(c);
                        reasons.insert(
                            c,
                            DirectReason::ExternalRepoTarget {
                                repo: repo.clone(),
                                via: via.clone(),
                            },
                        );
                        added = true;
                    }
                }
            }
        }
        if !added {
            // The repositories are accounted for and nothing new is direct, so
            // walking again would return the set it just returned.
            return found;
        }
        direct.sort_unstable();
        *result = walk::walk_view(view, direct, wopts);
    }
}

/// The direct set (spec 6.7), over either a materialised index or an overlay.
pub(crate) fn derive_direct<V: GraphView + ?Sized>(
    view: &V,
    inputs: &DirectInputs<'_>,
) -> (Vec<u32>, FxHashMap<u32, DirectReason>) {
    /// First reason wins: a target is direct once, for the first cause seen.
    fn add(
        direct: &mut FxHashSet<u32>,
        reasons: &mut FxHashMap<u32, DirectReason>,
        id: u32,
        reason: DirectReason,
    ) {
        if direct.insert(id) {
            reasons.insert(id, reason);
        }
    }

    let mut reasons: FxHashMap<u32, DirectReason> = FxHashMap::default();
    let mut direct: FxHashSet<u32> = FxHashSet::default();
    let mut scratch: Vec<u32> = Vec::new();

    for pd in &inputs.splice.packages {
        if inputs.bzl_dirty.contains(&pd.package) {
            // Spec 6.7's "bzl closure hash changed" branch, keyed on
            // membership of the stored load closure instead of a stored hash,
            // and load-bearing rather than an optimisation.
            //
            // A `.bzl` edit that changes a rule implementation without changing
            // any BUILD attribute leaves every rule proto in the loading
            // packages byte-identical — Bazel folds a rule's definition
            // environment into the proto, not the loading BUILD file's load
            // closure. So every rule hash comes back unchanged and `pd.changed`
            // is empty. Marking every rule in the package direct regardless of
            // its hash is the only thing that catches it, and
            // `Packages::bzl_closure_hash` is written as zero for exactly that
            // reason: the closure names the same set the hash comparison would
            // have, and names *which* packages, which a hash cannot.
            // (Covered by the `edit bzl` end-to-end scenario, which
            // asserts this is *why* those targets are reported.)
            for &id in &pd.current {
                if view.is_rule(id) {
                    let reason = DirectReason::BzlClosure {
                        package: pd.package.clone(),
                    };
                    add(&mut direct, &mut reasons, id, reason);
                }
            }
        } else {
            for &id in &pd.changed {
                add(&mut direct, &mut reasons, id, DirectReason::RuleHash);
            }
        }
        for &id in &pd.deleted {
            let label = view.label(id).to_string();
            for &consumer in view.reverse_row(id, &mut scratch) {
                let reason = DirectReason::DeletedDep {
                    label: label.clone(),
                };
                add(&mut direct, &mut reasons, consumer, reason);
            }
        }
    }

    for (pkg, ids) in inputs.deleted_by_package {
        for &id in ids {
            let label = view.label(id).to_string();
            tracing::debug!(package = %pkg, %label, "package deleted");
            for &consumer in view.reverse_row(id, &mut scratch) {
                let reason = DirectReason::DeletedDep {
                    label: label.clone(),
                };
                add(&mut direct, &mut reasons, consumer, reason);
            }
        }
    }

    // A file node's in-edges all come from rules, so its reverse row is its
    // consumer list.
    for (id, path) in inputs.changed_file_ids {
        for &consumer in view.reverse_row(*id, &mut scratch) {
            let reason = DirectReason::File { path: path.clone() };
            add(&mut direct, &mut reasons, consumer, reason);
        }
    }

    // An external repository's contents moved. The repository's targets are
    // leaves here — nothing ever queried them — so the change cannot be seen
    // as a rule-hash movement; what can be seen is every main-repo rule that
    // names one of its labels, which is exactly the leaf node's reverse row.
    for (id, label) in inputs.external_nodes {
        for &consumer in view.reverse_row(*id, &mut scratch) {
            let reason = DirectReason::ExternalRepo {
                label: label.clone(),
            };
            add(&mut direct, &mut reasons, consumer, reason);
        }
    }

    // A Starlark build setting's value moved. The setting itself is a target
    // whose *definition* did not change, so it is not direct; what changed is
    // every rule that reads it — the `config_setting`s whose `flag_values`
    // name it and the rules whose `attr.label` defaults point at it, which is
    // its reverse row. The `select()`s on those config_settings are edges too,
    // so the ordinary walk carries the answer outwards from there.
    for (id, label) in inputs.flag_nodes {
        for &consumer in view.reverse_row(*id, &mut scratch) {
            let reason = DirectReason::StarlarkFlag {
                label: label.clone(),
            };
            add(&mut direct, &mut reasons, consumer, reason);
        }
    }

    let mut direct: Vec<u32> = direct.into_iter().collect();
    direct.sort_unstable();
    (direct, reasons)
}

/// The affected list with each target's BFS depth, sorted by label.
pub(crate) fn affected_targets<V: GraphView + ?Sized>(
    view: &V,
    result: &walk::WalkResult,
) -> Vec<AffectedTarget> {
    let mut out: Vec<AffectedTarget> = result
        .affected
        .iter()
        .map(|&id| AffectedTarget {
            label: view.label(id).to_string(),
            depth: result.depth_of(id).unwrap_or(0),
            test: view.is_test(id),
        })
        .collect();
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out.dedup_by(|a, b| a.label == b.label);
    out
}

/// `--explain`: for each affected target, the shortest path back to the direct
/// member that reached it, and why that member was direct.
pub(crate) fn explain<V: GraphView + ?Sized>(
    view: &V,
    result: &walk::WalkResult,
    reasons: &FxHashMap<u32, DirectReason>,
) -> Vec<Explanation> {
    let mut out = Vec::with_capacity(result.affected.len());
    for &id in &result.affected {
        let Some(path) = result.path_to_seed(id) else {
            continue;
        };
        let seed = *path.last().unwrap_or(&id);
        let reason = reasons
            .get(&seed)
            .cloned()
            .unwrap_or(DirectReason::RuleHash);
        out.push(Explanation {
            target: view.label(id).to_string(),
            path: path.iter().map(|&i| view.label(i).to_string()).collect(),
            direct: view.label(seed).to_string(),
            reason,
        });
    }
    out.sort_by(|a, b| a.target.cmp(&b.target));
    out
}
