//! Mutations and commits.
//!
//! A `Commit` is a set of mutations plus the `git diff --name-status` the same
//! edit would have produced. Applying one to a `SynthGraph` yields the graph at
//! the next revision, which is what lets the property tests compare the
//! incremental engine against a full rebuild without ever running git or Bazel.

use rand::prelude::*;
use rand_chacha::ChaCha8Rng;

use cambium_diff::{Change, Status};

use crate::graph::{Kind, SynthGraph, SynthPackage, SynthTarget};
use crate::spec::Dist;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mutation {
    /// Edit a file's contents. Cannot change the graph.
    ModifySource {
        target: u32,
        slot: u32,
    },
    /// Add a file that the target's glob picks up.
    AddSource {
        target: u32,
    },
    DeleteSource {
        target: u32,
        slot: u32,
    },
    /// Change an attribute that is neither deps nor srcs.
    EditRuleAttr {
        target: u32,
    },
    AddDep {
        target: u32,
        dep: u32,
    },
    RemoveDep {
        target: u32,
        dep: u32,
    },
    AddTarget {
        package: u32,
        deps: Vec<u32>,
    },
    DeleteTarget {
        target: u32,
    },
    /// Bump a `.bzl` file, changing the rule implementations that load it.
    EditBzl {
        bzl: u32,
    },
    /// Move some of a package's targets into a new sub-package.
    SplitPackage {
        package: u32,
        targets: Vec<u32>,
    },
    /// Fold a sub-package back into its parent.
    MergePackage {
        package: u32,
    },
    /// Touch a global invalidator.
    Global,
}

/// Relative frequencies for the mutation kinds.
#[derive(Clone, Debug, PartialEq)]
pub struct MutationWeights {
    pub modify_source: u32,
    pub add_source: u32,
    pub delete_source: u32,
    pub edit_rule_attr: u32,
    pub add_dep: u32,
    pub remove_dep: u32,
    pub add_target: u32,
    pub delete_target: u32,
    pub edit_bzl: u32,
    pub split_package: u32,
    pub merge_package: u32,
    pub global: u32,
}

impl Default for MutationWeights {
    fn default() -> Self {
        // Roughly the mix of a real pull request: mostly file edits, some BUILD
        // churn, the structural changes rare but present.
        Self {
            modify_source: 60,
            add_source: 8,
            delete_source: 5,
            edit_rule_attr: 8,
            add_dep: 6,
            remove_dep: 4,
            add_target: 3,
            delete_target: 2,
            edit_bzl: 2,
            split_package: 1,
            merge_package: 1,
            global: 0,
        }
    }
}

impl MutationWeights {
    fn table(&self) -> Vec<(MutKind, u32)> {
        use MutKind::*;
        vec![
            (ModifySource, self.modify_source),
            (AddSource, self.add_source),
            (DeleteSource, self.delete_source),
            (EditRuleAttr, self.edit_rule_attr),
            (AddDep, self.add_dep),
            (RemoveDep, self.remove_dep),
            (AddTarget, self.add_target),
            (DeleteTarget, self.delete_target),
            (EditBzl, self.edit_bzl),
            (SplitPackage, self.split_package),
            (MergePackage, self.merge_package),
            (Global, self.global),
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MutKind {
    ModifySource,
    AddSource,
    DeleteSource,
    EditRuleAttr,
    AddDep,
    RemoveDep,
    AddTarget,
    DeleteTarget,
    EditBzl,
    SplitPackage,
    MergePackage,
    Global,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChangeSpec {
    pub seed: u64,
    /// Mutations per commit.
    pub files_changed: Dist,
    pub weights: MutationWeights,
    /// Probability that a mutation picks a hub target rather than a random one.
    pub hub_bias: f64,
}

impl Default for ChangeSpec {
    fn default() -> Self {
        Self {
            seed: 1,
            files_changed: Dist::LogNormal {
                median: 6.0,
                sigma: 0.9,
                min: 1,
                max: 200,
            },
            weights: MutationWeights::default(),
            hub_bias: 0.05,
        }
    }
}

impl ChangeSpec {
    /// Named presets, one per kind of change a benchmark wants to measure.
    /// [`Self::PRESETS`] lists them.
    pub fn preset(name: &str) -> Option<ChangeSpec> {
        let mut c = ChangeSpec::default();
        match name {
            "typical-pr" => {
                // Mostly file edits, some BUILD churn. Structural changes are
                // deliberately absent: splitting a package rewrites every
                // dependent's BUILD file, which is a refactor, not a typical
                // pull request, and it belongs in the presets named for it.
                c.files_changed = Dist::Uniform { lo: 3, hi: 20 };
                c.weights = MutationWeights {
                    split_package: 0,
                    merge_package: 0,
                    ..MutationWeights::default()
                };
            }
            "build-edit" => {
                c.files_changed = Dist::Uniform { lo: 1, hi: 5 };
                c.weights = MutationWeights {
                    modify_source: 0,
                    edit_rule_attr: 40,
                    add_dep: 30,
                    remove_dep: 20,
                    add_target: 10,
                    ..MutationWeights::default()
                };
            }
            "bzl-edit" => {
                c.files_changed = Dist::Uniform { lo: 1, hi: 3 };
                c.weights = MutationWeights {
                    modify_source: 0,
                    edit_bzl: 100,
                    ..MutationWeights::default()
                };
            }
            "hub-touch" => {
                c.files_changed = Dist::Uniform { lo: 1, hi: 4 };
                c.hub_bias = 1.0;
            }
            "refactor" => {
                c.files_changed = Dist::Uniform { lo: 150, hi: 250 };
            }
            "package-split" => {
                c.files_changed = Dist::Uniform { lo: 1, hi: 2 };
                c.weights = MutationWeights {
                    modify_source: 0,
                    split_package: 60,
                    merge_package: 40,
                    ..MutationWeights::default()
                };
            }
            "pathological" => {
                c.files_changed = Dist::Uniform { lo: 20, hi: 40 };
                c.hub_bias = 1.0;
                c.weights = MutationWeights {
                    modify_source: 30,
                    edit_bzl: 40,
                    edit_rule_attr: 30,
                    ..MutationWeights::default()
                };
            }
            "mixed" => {}
            _ => return None,
        }
        Some(c)
    }

    pub const PRESETS: &[&str] = &[
        "typical-pr",
        "build-edit",
        "bzl-edit",
        "hub-touch",
        "refactor",
        "package-split",
        "pathological",
        "mixed",
    ];
}

/// One synthetic revision.
#[derive(Clone, Debug, Default)]
pub struct Commit {
    pub mutations: Vec<Mutation>,
    /// What `git diff --name-status base head` would report.
    pub changes: Vec<Change>,
}

/// Generates and applies a commit, returning what changed on disk.
pub fn commit(g: &mut SynthGraph, spec: &ChangeSpec, revision: u64) -> Commit {
    // Seeding per revision keeps a sequence of commits reproducible whether it
    // is replayed from the start or resumed.
    let mut rng =
        ChaCha8Rng::seed_from_u64(spec.seed ^ (revision.wrapping_mul(0x9E37_79B9_7F4A_7C15)));
    let n = spec.files_changed.sample(&mut rng).max(1);
    let table = spec.weights.table();
    let total: u32 = table.iter().map(|(_, w)| w).sum();

    let mut out = Commit::default();
    if total == 0 {
        return out;
    }
    for _ in 0..n {
        let mut pick = rng.gen_range(0..total);
        let mut kind = table[0].0;
        for (k, w) in &table {
            if pick < *w {
                kind = *k;
                break;
            }
            pick -= *w;
        }
        if let Some(m) = generate(g, kind, spec, &mut rng) {
            apply(g, &m, &mut out.changes);
            out.mutations.push(m);
        }
    }
    normalise(&mut out.changes);
    out
}

/// Applies a specific mutation, for hand-written scenarios.
pub fn apply_one(g: &mut SynthGraph, m: &Mutation) -> Vec<Change> {
    let mut changes = Vec::new();
    apply(g, m, &mut changes);
    normalise(&mut changes);
    changes
}

fn pick_target<R: Rng>(g: &SynthGraph, spec: &ChangeSpec, rng: &mut R) -> Option<u32> {
    if g.targets.is_empty() {
        return None;
    }
    // Hubs are the layer-0 targets everything depends on; biasing towards them
    // is how the `hub-touch` and `pathological` presets get their blast radius.
    if rng.gen::<f64>() < spec.hub_bias {
        let hubs: Vec<u32> = (0..g.targets.len() as u32)
            .filter(|&i| g.targets[i as usize].alive && g.targets[i as usize].layer == 0)
            .take(64)
            .collect();
        if !hubs.is_empty() {
            return Some(hubs[rng.gen_range(0..hubs.len())]);
        }
    }
    for _ in 0..32 {
        let i = rng.gen_range(0..g.targets.len()) as u32;
        if g.targets[i as usize].alive {
            return Some(i);
        }
    }
    None
}

fn generate<R: Rng>(
    g: &SynthGraph,
    kind: MutKind,
    spec: &ChangeSpec,
    rng: &mut R,
) -> Option<Mutation> {
    Some(match kind {
        MutKind::ModifySource => {
            let t = pick_target(g, spec, rng)?;
            let srcs = &g.targets[t as usize].srcs;
            if srcs.is_empty() {
                return None;
            }
            Mutation::ModifySource {
                target: t,
                slot: srcs[rng.gen_range(0..srcs.len())],
            }
        }
        MutKind::AddSource => Mutation::AddSource {
            target: pick_target(g, spec, rng)?,
        },
        MutKind::DeleteSource => {
            let t = pick_target(g, spec, rng)?;
            let srcs = &g.targets[t as usize].srcs;
            if srcs.is_empty() {
                return None;
            }
            Mutation::DeleteSource {
                target: t,
                slot: srcs[rng.gen_range(0..srcs.len())],
            }
        }
        MutKind::EditRuleAttr => {
            // An alias or a test_suite has no attribute the emitted BUILD file
            // carries the salt in, so editing one would move the in-memory
            // hash without changing anything git can see: an edit no real
            // repository can make. Pick a rule that has attributes.
            let target = (0..8)
                .filter_map(|_| pick_target(g, spec, rng))
                .find(|&t| !matches!(g.targets[t as usize].kind, Kind::Alias | Kind::TestSuite))?;
            Mutation::EditRuleAttr { target }
        }
        MutKind::AddDep => {
            let t = pick_target(g, spec, rng)?;
            let layer = g.targets[t as usize].layer;
            if layer == 0 {
                return None;
            }
            // Keep the DAG: only depend on strictly lower layers.
            for _ in 0..32 {
                let d = rng.gen_range(0..g.targets.len()) as u32;
                let dt = &g.targets[d as usize];
                if dt.alive && dt.layer < layer && !g.targets[t as usize].deps.contains(&d) {
                    return Some(Mutation::AddDep { target: t, dep: d });
                }
            }
            return None;
        }
        MutKind::RemoveDep => {
            let t = pick_target(g, spec, rng)?;
            let deps = &g.targets[t as usize].deps;
            if deps.is_empty() {
                return None;
            }
            Mutation::RemoveDep {
                target: t,
                dep: deps[rng.gen_range(0..deps.len())],
            }
        }
        MutKind::AddTarget => {
            let anchor = pick_target(g, spec, rng)?;
            let package = g.targets[anchor as usize].package;
            // A generated new target starts with no deps; `apply` gives it a
            // layer above everything already in the package, so a hand-written
            // scenario can add deps and still keep the DAG.
            Mutation::AddTarget {
                package,
                deps: Vec::new(),
            }
        }
        MutKind::DeleteTarget => Mutation::DeleteTarget {
            target: pick_target(g, spec, rng)?,
        },
        MutKind::EditBzl => {
            if g.bzl.is_empty() {
                return None;
            }
            Mutation::EditBzl {
                bzl: rng.gen_range(0..g.bzl.len()) as u32,
            }
        }
        MutKind::SplitPackage => {
            let anchor = pick_target(g, spec, rng)?;
            let package = g.targets[anchor as usize].package;
            let live: Vec<u32> = g.packages[package as usize]
                .targets
                .iter()
                .copied()
                .filter(|&t| g.targets[t as usize].alive)
                .collect();
            if live.len() < 2 {
                return None;
            }
            let take = (live.len() / 2).max(1);
            Mutation::SplitPackage {
                package,
                targets: live[..take].to_vec(),
            }
        }
        MutKind::MergePackage => {
            let anchor = pick_target(g, spec, rng)?;
            let package = g.targets[anchor as usize].package;
            // Only a package with a live parent package can be merged upward.
            let path = g.packages[package as usize].path.clone();
            let parent = cambium_index::label::parent_package(&path)?;
            if !g.pkg_by_path.contains_key(parent) {
                return None;
            }
            Mutation::MergePackage { package }
        }
        MutKind::Global => Mutation::Global,
    })
}

fn apply(g: &mut SynthGraph, m: &Mutation, changes: &mut Vec<Change>) {
    match m {
        Mutation::ModifySource { target, slot } => {
            let path = g.src_path(*target, *slot);
            let t = &mut g.targets[*target as usize];
            if let Some(i) = t.srcs.iter().position(|s| s == slot) {
                t.src_versions[i] += 1;
                changes.push(Change::new(Status::Modified, path));
            }
        }
        Mutation::AddSource { target } => {
            let t = &mut g.targets[*target as usize];
            let slot = t.srcs.iter().copied().max().map(|m| m + 1).unwrap_or(0);
            t.srcs.push(slot);
            t.src_versions.push(0);
            changes.push(Change::new(Status::Added, g.src_path(*target, slot)));
        }
        Mutation::DeleteSource { target, slot } => {
            let path = g.src_path(*target, *slot);
            let t = &mut g.targets[*target as usize];
            if let Some(i) = t.srcs.iter().position(|s| s == slot) {
                t.srcs.remove(i);
                t.src_versions.remove(i);
                changes.push(Change::new(Status::Deleted, path));
                // If the file was exported, its consumers' BUILD files just
                // lost a src, and the exporting package its exports_files line.
                unexport(g, *target, Some(*slot), changes);
            }
        }
        Mutation::EditRuleAttr { target } => {
            g.targets[*target as usize].attr_salt += 1;
            let pkg = g.targets[*target as usize].package;
            changes.push(Change::new(Status::Modified, g.build_path(pkg)));
        }
        Mutation::AddDep { target, dep } => {
            let t = &mut g.targets[*target as usize];
            if !t.deps.contains(dep) {
                t.deps.push(*dep);
            }
            let pkg = g.targets[*target as usize].package;
            changes.push(Change::new(Status::Modified, g.build_path(pkg)));
        }
        Mutation::RemoveDep { target, dep } => {
            let t = &mut g.targets[*target as usize];
            t.deps.retain(|d| d != dep);
            let pkg = g.targets[*target as usize].package;
            changes.push(Change::new(Status::Modified, g.build_path(pkg)));
        }
        Mutation::AddTarget { package, deps } => add_target(g, *package, deps, changes),
        Mutation::DeleteTarget { target } => delete_target(g, *target, changes),
        Mutation::EditBzl { bzl } => {
            g.bzl[*bzl as usize].version += 1;
            changes.push(Change::new(
                Status::Modified,
                g.bzl[*bzl as usize].path.clone(),
            ));
        }
        Mutation::SplitPackage { package, targets } => split_package(g, *package, targets, changes),
        Mutation::MergePackage { package } => merge_package(g, *package, changes),
        Mutation::Global => {
            changes.push(Change::new(Status::Modified, "MODULE.bazel".to_string()));
        }
    }
}

/// A new `Lib` with one source, one layer above everything alive in its
/// package, keeping only the deps that keep the DAG.
fn add_target(g: &mut SynthGraph, package: u32, deps: &[u32], changes: &mut Vec<Change>) {
    let id = g.targets.len() as u32;
    let layer = g.packages[package as usize]
        .targets
        .iter()
        .filter(|&&t| g.targets[t as usize].alive)
        .map(|&t| g.targets[t as usize].layer)
        .max()
        .unwrap_or(0)
        + 1;
    let deps: Vec<u32> = deps
        .iter()
        .copied()
        .filter(|&d| g.targets[d as usize].alive && g.targets[d as usize].layer < layer)
        .collect();
    g.targets.push(SynthTarget {
        package,
        kind: Kind::Lib,
        layer,
        deps,
        select_deps: 0,
        ext_deps: 0,
        srcs: vec![0],
        src_versions: vec![0],
        foreign_srcs: Vec::new(),
        attr_salt: 0,
        tags: 0,
        alive: true,
    });
    g.packages[package as usize].targets.push(id);
    changes.push(Change::new(Status::Added, g.src_path(id, 0)));
    changes.push(Change::new(Status::Modified, g.build_path(package)));
}

/// Kills a target: its sources go, its BUILD file and every dependent's
/// BUILD file change, and nothing may still export or consume its files.
fn delete_target(g: &mut SynthGraph, target: u32, changes: &mut Vec<Change>) {
    let pkg = g.targets[target as usize].package;
    let srcs = g.targets[target as usize].srcs.clone();
    g.targets[target as usize].alive = false;
    // Dependents keep a dangling dep until they are rebuilt; the
    // package_update filters dead deps, which is what a real BUILD edit
    // would have done.
    for &s in &srcs {
        changes.push(Change::new(Status::Deleted, g.src_path(target, s)));
    }
    changes.push(Change::new(Status::Modified, g.build_path(pkg)));
    // Every package holding a dependent had to drop the reference.
    for p in dependent_packages(g, &[target]) {
        changes.push(Change::new(Status::Modified, g.build_path(p)));
    }
    // Its files are gone too: a consumer's `srcs` naming a deleted file is a
    // BUILD error, and the reference oracle would read a dead target's
    // versions.
    unexport(g, target, None, changes);
}

/// Moves `targets` out of `package` into a fresh `sub<n>` package under it,
/// which loads what its parent loads.
fn split_package(g: &mut SynthGraph, package: u32, targets: &[u32], changes: &mut Vec<Change>) {
    let parent_path = g.packages[package as usize].path.clone();
    let mut i = 0;
    let new_path = loop {
        let p = format!("{parent_path}/sub{i}");
        if !g.pkg_by_path.contains_key(&p) {
            break p;
        }
        i += 1;
    };
    let new_id = g.packages.len() as u32;
    let bzl = g.packages[package as usize].bzl.clone();
    g.packages.push(SynthPackage {
        path: new_path.clone(),
        targets: Vec::new(),
        bzl: bzl.clone(),
        exported: Vec::new(),
        alive: true,
    });
    for &b in &bzl {
        g.bzl[b as usize].loaded_by.push(new_id);
    }
    g.pkg_by_path.insert(new_path.clone(), new_id);

    let moved: Vec<u32> = targets
        .iter()
        .copied()
        .filter(|&t| g.targets[t as usize].alive && g.targets[t as usize].package == package)
        .collect();
    move_targets(g, &moved, new_id, changes);
    changes.push(Change::new(Status::Added, g.build_path(new_id)));
    changes.push(Change::new(Status::Modified, g.build_path(package)));
}

/// Folds `package` into its parent package: its BUILD file is deleted and
/// its live targets move up. A package with no live parent stays as it is.
fn merge_package(g: &mut SynthGraph, package: u32, changes: &mut Vec<Change>) {
    let path = g.packages[package as usize].path.clone();
    let Some(parent) = cambium_index::label::parent_package(&path) else {
        return;
    };
    let Some(&parent_id) = g.pkg_by_path.get(parent) else {
        return;
    };
    let moved: Vec<u32> = g.packages[package as usize]
        .targets
        .iter()
        .copied()
        .filter(|&t| g.targets[t as usize].alive)
        .collect();
    changes.push(Change::new(Status::Deleted, g.build_path(package)));
    move_targets(g, &moved, parent_id, changes);
    g.packages[package as usize].alive = false;
    g.packages[package as usize].targets.clear();
    g.pkg_by_path.remove(&path);
    changes.push(Change::new(Status::Modified, g.build_path(parent_id)));
}

/// Moves targets (and their source files) into another package.
fn move_targets(g: &mut SynthGraph, moved: &[u32], dest: u32, changes: &mut Vec<Change>) {
    for &t in moved {
        let srcs = g.targets[t as usize].srcs.clone();
        let old_paths: Vec<String> = srcs.iter().map(|&s| g.src_path(t, s)).collect();
        let from = g.targets[t as usize].package;
        g.packages[from as usize].targets.retain(|&x| x != t);
        g.targets[t as usize].package = dest;
        g.packages[dest as usize].targets.push(t);
        for (i, &s) in srcs.iter().enumerate() {
            changes.push(Change::new(Status::Deleted, old_paths[i].clone()));
            changes.push(Change::new(Status::Added, g.src_path(t, s)));
        }
        // An exported file moves with its owner: the `exports_files` line goes
        // to the destination package, and every consumer's record of which
        // package the file lives in follows it.
        let exported: Vec<(u32, u32)> = g.packages[from as usize]
            .exported
            .iter()
            .copied()
            .filter(|&(et, _)| et == t)
            .collect();
        if !exported.is_empty() {
            g.packages[from as usize]
                .exported
                .retain(|&(et, _)| et != t);
            g.packages[dest as usize].exported.extend(exported);
            for c in g.targets.iter_mut() {
                for f in c.foreign_srcs.iter_mut() {
                    if f.1 == t {
                        f.0 = dest;
                    }
                }
            }
        }
    }
    // Anyone referring to a moved target now refers to a different label, so
    // their BUILD files changed too.
    for p in dependent_packages(g, moved) {
        changes.push(Change::new(Status::Modified, g.build_path(p)));
    }
}

/// Drops `target`'s exported files (one slot, or all of them) from its
/// package's `exports_files` and from every consumer's `srcs`, recording the
/// BUILD edits that implies.
fn unexport(g: &mut SynthGraph, target: u32, slot: Option<u32>, changes: &mut Vec<Change>) {
    let gone = |t: u32, s: u32| t == target && slot.is_none_or(|slot| slot == s);
    let pkg = g.targets[target as usize].package;
    let exported = &mut g.packages[pkg as usize].exported;
    let before = exported.len();
    exported.retain(|&(t, s)| !gone(t, s));
    if exported.len() == before {
        return;
    }
    changes.push(Change::new(Status::Modified, g.build_path(pkg)));
    let mut consumers: Vec<u32> = Vec::new();
    for (id, c) in g.targets.iter_mut().enumerate() {
        let before = c.foreign_srcs.len();
        c.foreign_srcs.retain(|&(_, t, s)| !gone(t, s));
        if c.alive && c.foreign_srcs.len() != before {
            consumers.push(id as u32);
        }
    }
    for c in consumers {
        changes.push(Change::new(
            Status::Modified,
            g.build_path(g.targets[c as usize].package),
        ));
    }
}

/// Packages holding a live target that names one of `targets`, as a dep or as
/// the owner of a file it consumes via `exports_files`.
fn dependent_packages(g: &SynthGraph, targets: &[u32]) -> Vec<u32> {
    let set: std::collections::HashSet<u32> = targets.iter().copied().collect();
    let mut pkgs: Vec<u32> = g
        .targets
        .iter()
        .filter(|t| {
            t.alive
                && (t.deps.iter().any(|d| set.contains(d))
                    || t.foreign_srcs.iter().any(|&(_, ft, _)| set.contains(&ft)))
        })
        .map(|t| t.package)
        .collect();
    pkgs.sort_unstable();
    pkgs.dedup();
    pkgs
}

/// Folds a sequence of per-mutation edits into the diff `git` would report
/// between the two revisions.
///
/// Order matters, and getting it wrong hides bugs: a file added and then edited
/// in the same range is *added* as far as `git diff base head` is concerned,
/// not modified, and only the added form dirties the package that globs it. A
/// file added and then deleted never existed at either end and is not reported
/// at all.
fn normalise(changes: &mut Vec<Change>) {
    use std::collections::BTreeMap;
    let mut first: BTreeMap<String, Status> = BTreeMap::new();
    let mut last: BTreeMap<String, Status> = BTreeMap::new();
    for c in changes.drain(..) {
        first.entry(c.path.clone()).or_insert(c.status);
        last.insert(c.path, c.status);
    }
    for (path, first_status) in first {
        let existed_at_base = first_status != Status::Added;
        let exists_at_head = last[&path] != Status::Deleted;
        let status = match (existed_at_base, exists_at_head) {
            (false, false) => continue,
            (false, true) => Status::Added,
            (true, false) => Status::Deleted,
            (true, true) => Status::Modified,
        };
        changes.push(Change { status, path });
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::GraphSpec;

    fn graph() -> SynthGraph {
        crate::generate(&GraphSpec::preset("small").unwrap())
    }

    #[test]
    fn commits_are_deterministic() {
        let spec = ChangeSpec::default();
        let (mut a, mut b) = (graph(), graph());
        for rev in 0..5 {
            let ca = commit(&mut a, &spec, rev);
            let cb = commit(&mut b, &spec, rev);
            assert_eq!(ca.changes, cb.changes, "revision {rev}");
            assert_eq!(ca.mutations, cb.mutations);
        }
    }

    #[test]
    fn mutations_preserve_the_dag() {
        let spec = ChangeSpec {
            weights: MutationWeights {
                add_dep: 100,
                ..MutationWeights::default()
            },
            ..Default::default()
        };
        let mut g = graph();
        for rev in 0..10 {
            commit(&mut g, &spec, rev);
        }
        for t in g.targets.iter().filter(|t| t.alive) {
            for &d in &t.deps {
                assert!(g.targets[d as usize].layer < t.layer);
            }
        }
    }

    #[test]
    fn modify_source_bumps_content_only() {
        let mut g = graph();
        let t = (0..g.targets.len() as u32)
            .find(|&i| !g.targets[i as usize].srcs.is_empty())
            .unwrap();
        let slot = g.targets[t as usize].srcs[0];
        let before = g.target_hash(t);
        let changes = apply_one(&mut g, &Mutation::ModifySource { target: t, slot });
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].status, Status::Modified);
        // The rule definition is untouched: only the file content moved.
        assert_eq!(before, g.target_hash(t));
        assert_eq!(g.src_version(t, slot), 1);
    }

    #[test]
    fn add_source_changes_the_rule_hash() {
        let mut g = graph();
        let before = g.target_hash(0);
        let changes = apply_one(&mut g, &Mutation::AddSource { target: 0 });
        assert_eq!(changes[0].status, Status::Added);
        assert_ne!(before, g.target_hash(0));
    }

    #[test]
    fn splitting_a_package_moves_targets_and_files() {
        let mut g = graph();
        let pkg = g.targets[0].package;
        let live: Vec<u32> = g.packages[pkg as usize]
            .targets
            .iter()
            .copied()
            .filter(|&t| g.targets[t as usize].alive)
            .collect();
        if live.len() < 2 {
            return;
        }
        let moved = live[..1].to_vec();
        let changes = apply_one(
            &mut g,
            &Mutation::SplitPackage {
                package: pkg,
                targets: moved.clone(),
            },
        );
        let new_pkg = g.targets[moved[0] as usize].package;
        assert_ne!(new_pkg, pkg);
        assert!(g.package_path(new_pkg).starts_with(g.package_path(pkg)));
        assert!(changes
            .iter()
            .any(|c| c.status == Status::Added && c.path.ends_with("BUILD.bazel")));
    }

    #[test]
    fn merging_a_package_deletes_its_build_file() {
        let mut g = graph();
        let pkg = g.targets[0].package;
        let live: Vec<u32> = g.packages[pkg as usize].targets.clone();
        apply_one(
            &mut g,
            &Mutation::SplitPackage {
                package: pkg,
                targets: live,
            },
        );
        let sub = *g
            .pkg_by_path
            .iter()
            .find(|(p, _)| p.ends_with("/sub0"))
            .unwrap()
            .1;
        let changes = apply_one(&mut g, &Mutation::MergePackage { package: sub });
        assert!(!g.packages[sub as usize].alive);
        assert!(changes
            .iter()
            .any(|c| c.status == Status::Deleted && c.path.ends_with("BUILD.bazel")));
    }

    #[test]
    fn change_folding_matches_git_semantics() {
        let mut g = graph();
        let t = (0..g.targets.len() as u32)
            .find(|&i| !g.targets[i as usize].srcs.is_empty())
            .unwrap();
        // Add a file and then edit it: git reports an add.
        let mut changes = Vec::new();
        apply(&mut g, &Mutation::AddSource { target: t }, &mut changes);
        let slot = *g.targets[t as usize].srcs.last().unwrap();
        apply(
            &mut g,
            &Mutation::ModifySource { target: t, slot },
            &mut changes,
        );
        normalise(&mut changes);
        let path = g.src_path(t, slot);
        assert_eq!(
            changes.iter().find(|c| c.path == path).map(|c| c.status),
            Some(Status::Added)
        );

        // Add a file and then delete it: git reports nothing.
        let mut changes = Vec::new();
        apply(&mut g, &Mutation::AddSource { target: t }, &mut changes);
        let slot = *g.targets[t as usize].srcs.last().unwrap();
        apply(
            &mut g,
            &Mutation::DeleteSource { target: t, slot },
            &mut changes,
        );
        normalise(&mut changes);
        assert!(changes.iter().all(|c| c.path != g.src_path(t, slot)));

        // Edit and then delete: git reports a delete.
        let slot = g.targets[t as usize].srcs[0];
        let path = g.src_path(t, slot);
        let mut changes = Vec::new();
        apply(
            &mut g,
            &Mutation::ModifySource { target: t, slot },
            &mut changes,
        );
        apply(
            &mut g,
            &Mutation::DeleteSource { target: t, slot },
            &mut changes,
        );
        normalise(&mut changes);
        assert_eq!(
            changes.iter().find(|c| c.path == path).map(|c| c.status),
            Some(Status::Deleted)
        );
    }

    /// A package that exports a file, the target that owns it, and one
    /// consumer of it.
    fn an_export(g: &SynthGraph) -> (u32, u32, u32, u32) {
        let (pkg, &(et, slot)) = g
            .packages
            .iter()
            .enumerate()
            .find_map(|(i, p)| p.exported.first().map(|e| (i as u32, e)))
            .expect("the small preset exports something");
        let consumer = (0..g.targets.len() as u32)
            .find(|&c| {
                g.targets[c as usize]
                    .foreign_srcs
                    .iter()
                    .any(|&(_, t, s)| t == et && s == slot)
            })
            .expect("an exported file has a consumer");
        (pkg, et, slot, consumer)
    }

    /// Every `exports_files` entry names a live target's live slot in that very
    /// package, and every consumer's foreign src is exported by the package it
    /// says it is.
    fn assert_exports_consistent(g: &SynthGraph) {
        for (pi, p) in g.packages.iter().enumerate() {
            for &(t, s) in &p.exported {
                let owner = &g.targets[t as usize];
                assert!(owner.alive, "//{} exports dead t{t}_{s}.src", p.path);
                assert_eq!(
                    owner.package, pi as u32,
                    "//{} exports a file it does not own",
                    p.path
                );
                assert!(
                    owner.srcs.contains(&s),
                    "//{} exports a deleted slot",
                    p.path
                );
            }
        }
        for c in g.targets.iter().filter(|t| t.alive) {
            for &(fp, ft, fs) in &c.foreign_srcs {
                assert!(
                    g.targets[ft as usize].alive,
                    "a consumer still names dead t{ft}"
                );
                assert_eq!(g.targets[ft as usize].package, fp);
                assert!(g.packages[fp as usize].exported.contains(&(ft, fs)));
            }
        }
    }

    #[test]
    fn deleting_an_exporter_drops_its_consumers_foreign_srcs() {
        let mut g = graph();
        let (pkg, et, _, consumer) = an_export(&g);
        let consumer_pkg = g.targets[consumer as usize].package;
        let changes = apply_one(&mut g, &Mutation::DeleteTarget { target: et });
        assert!(g.packages[pkg as usize]
            .exported
            .iter()
            .all(|&(t, _)| t != et));
        assert!(g.targets[consumer as usize]
            .foreign_srcs
            .iter()
            .all(|&(_, t, _)| t != et));
        // The consumer's BUILD file lost a src, so git saw it change.
        let build = g.build_path(consumer_pkg);
        assert!(changes.iter().any(|c| c.path == build), "{changes:?}");
        assert_exports_consistent(&g);
        // The oracle must not need a dead target's file versions.
        crate::reference::deep_hashes(&g);
    }

    #[test]
    fn deleting_an_exported_file_unexports_it() {
        let mut g = graph();
        let (pkg, et, slot, consumer) = an_export(&g);
        apply_one(&mut g, &Mutation::DeleteSource { target: et, slot });
        assert!(!g.packages[pkg as usize].exported.contains(&(et, slot)));
        assert!(g.targets[consumer as usize]
            .foreign_srcs
            .iter()
            .all(|&(_, t, s)| (t, s) != (et, slot)));
        assert_exports_consistent(&g);
    }

    #[test]
    fn splitting_moves_exports_with_their_owner() {
        let mut g = graph();
        let (pkg, et, slot, consumer) = an_export(&g);
        let consumer_pkg = g.targets[consumer as usize].package;
        let changes = apply_one(
            &mut g,
            &Mutation::SplitPackage {
                package: pkg,
                targets: vec![et],
            },
        );
        let new_pkg = g.targets[et as usize].package;
        assert_ne!(new_pkg, pkg);
        assert!(!g.packages[pkg as usize].exported.contains(&(et, slot)));
        assert!(g.packages[new_pkg as usize].exported.contains(&(et, slot)));
        assert!(g.targets[consumer as usize]
            .foreign_srcs
            .contains(&(new_pkg, et, slot)));
        // The consumer's srcs label changed, so its BUILD file did.
        let build = g.build_path(consumer_pkg);
        assert!(changes.iter().any(|c| c.path == build), "{changes:?}");
        assert_exports_consistent(&g);

        // And merging it back restores the original owner.
        apply_one(&mut g, &Mutation::MergePackage { package: new_pkg });
        assert!(g.packages[pkg as usize].exported.contains(&(et, slot)));
        assert_exports_consistent(&g);
    }

    #[test]
    fn a_long_mixed_history_keeps_exports_consistent() {
        let spec = ChangeSpec {
            weights: MutationWeights {
                delete_target: 20,
                delete_source: 20,
                split_package: 20,
                merge_package: 20,
                ..MutationWeights::default()
            },
            ..ChangeSpec::default()
        };
        let mut g = graph();
        for rev in 0..40 {
            commit(&mut g, &spec, rev);
            assert_exports_consistent(&g);
        }
    }

    #[test]
    fn presets_resolve() {
        for p in ChangeSpec::PRESETS {
            assert!(ChangeSpec::preset(p).is_some(), "{p}");
        }
    }
}
