//! Deterministic synthetic target graph.
//!
//! Generation is O(targets + edges) and reproducible from the seed: same
//! `GraphSpec`, same graph. Labels are derived from ids rather than stored, so
//! a 10M-target graph fits in memory.

use rand::prelude::*;
use rand_chacha::ChaCha8Rng;
use rustc_hash::FxHashMap;

use cambium_index::{NodeKind, PackageUpdate, TargetRow};

use crate::spec::GraphSpec;

/// Rule flavour. Maps onto the rule classes the Bazel emitter defines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Lib,
    Test,
    Bin,
    Alias,
    TestSuite,
}

impl Kind {
    pub fn rule_class(self) -> &'static str {
        match self {
            Kind::Lib => "synth_lib",
            Kind::Test => "synth_test",
            Kind::Bin => "synth_bin",
            Kind::Alias => "alias",
            Kind::TestSuite => "test_suite",
        }
    }

    pub fn is_test(self) -> bool {
        matches!(self, Kind::Test | Kind::TestSuite)
    }
}

/// Tag bits.
pub const TAG_MANUAL: u8 = 1 << 0;

/// Size of the pool of `@synth_ext//ext:lib<n>` targets. [`SynthGraph::ext_label`]
/// draws from it and the emitter writes exactly this many, so the two cannot
/// disagree about which external labels exist.
pub(crate) const EXT_LIBS: u32 = 64;

#[derive(Clone, Debug)]
pub struct SynthTarget {
    pub package: u32,
    pub kind: Kind,
    pub layer: u32,
    /// Same-repo rule deps.
    pub deps: Vec<u32>,
    /// The last `select_deps` entries of `deps` are emitted inside a `select()`.
    pub select_deps: u16,
    /// Number of `@synth_ext//...` deps; their labels are derived from the id.
    pub ext_deps: u16,
    /// Source file slots owned by this target: `t<id>_<slot>.src`.
    pub srcs: Vec<u32>,
    /// Content version per entry of `srcs`, bumped by `ModifySource`. Not part
    /// of the target hash: editing a file's contents cannot change the graph,
    /// only mark the file's consumers direct (spec section 2).
    pub src_versions: Vec<u32>,
    /// `(package, target, slot)` triples for files consumed via `exports_files`.
    pub foreign_srcs: Vec<(u32, u32, u32)>,
    /// Bumped by `EditRuleAttr`; folded into the content hash.
    pub attr_salt: u32,
    pub tags: u8,
    pub alive: bool,
}

impl SynthTarget {
    pub fn is_manual(&self) -> bool {
        self.tags & TAG_MANUAL != 0
    }
}

#[derive(Clone, Debug)]
pub struct SynthPackage {
    pub path: String,
    pub targets: Vec<u32>,
    pub bzl: Vec<u32>,
    /// `(target, slot)` files this package makes visible via `exports_files`.
    pub exported: Vec<(u32, u32)>,
    pub alive: bool,
}

#[derive(Clone, Debug)]
pub struct SynthBzl {
    pub path: String,
    /// Bumped by `EditBzl`; folded into every loading package's bzl hash.
    pub version: u32,
    pub loaded_by: Vec<u32>,
}

/// A source slot's file name, `t<id>_<slot>.src`: the one spelling every
/// label, path and hash of a source file is built from. Formatting it in
/// place keeps the hashing path free of a string per file.
struct SrcName(u32, u32);

impl std::fmt::Display for SrcName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "t{}_{}.src", self.0, self.1)
    }
}

#[derive(Clone, Debug)]
pub struct SynthGraph {
    pub packages: Vec<SynthPackage>,
    pub targets: Vec<SynthTarget>,
    pub bzl: Vec<SynthBzl>,
    pub pkg_by_path: FxHashMap<String, u32>,
    /// `SynthBzl::path` -> index into `bzl`, for the in-memory file server.
    pub bzl_by_path: FxHashMap<String, u32>,
}

impl SynthGraph {
    // ---- naming ----

    pub fn package_path(&self, pkg: u32) -> &str {
        &self.packages[pkg as usize].path
    }

    pub fn target_label(&self, id: u32) -> String {
        let mut out = String::new();
        self.write_target_label(id, &mut out);
        out
    }

    pub fn src_name(target: u32, slot: u32) -> String {
        SrcName(target, slot).to_string()
    }

    pub fn src_label(&self, target: u32, slot: u32) -> String {
        let mut out = String::new();
        self.write_src_label(target, slot, &mut out);
        out
    }

    pub fn src_path(&self, target: u32, slot: u32) -> String {
        let p = self.package_path(self.targets[target as usize].package);
        if p.is_empty() {
            Self::src_name(target, slot)
        } else {
            format!("{}/{}", p, Self::src_name(target, slot))
        }
    }

    pub fn build_path(&self, pkg: u32) -> String {
        let p = self.package_path(pkg);
        if p.is_empty() {
            "BUILD.bazel".to_string()
        } else {
            format!("{p}/BUILD.bazel")
        }
    }

    pub fn ext_label(target: u32, k: u16) -> String {
        // Spread external deps over a fixed-size pool so they are shared.
        let h = (target.wrapping_mul(2_654_435_761).wrapping_add(k as u32)) % EXT_LIBS;
        format!("@synth_ext//ext:lib{h}")
    }

    /// Content version of one source file, for the reference algorithm.
    pub fn src_version(&self, target: u32, slot: u32) -> u32 {
        let t = &self.targets[target as usize];
        t.srcs
            .iter()
            .position(|&s| s == slot)
            .and_then(|i| t.src_versions.get(i).copied())
            .unwrap_or(0)
    }

    pub fn num_alive_targets(&self) -> usize {
        self.targets.iter().filter(|t| t.alive).count()
    }

    // ---- hashing ----

    /// Hash of a target's definition, standing in for the rule proto hash.
    /// Changes exactly when the target's own definition changes.
    pub fn target_hash(&self, id: u32) -> [u8; 32] {
        let mut scratch = String::new();
        self.target_hash_into(id, &mut scratch)
    }

    fn write_target_label(&self, id: u32, out: &mut String) {
        use std::fmt::Write as _;
        out.clear();
        let _ = write!(
            out,
            "//{}:t{}",
            self.package_path(self.targets[id as usize].package),
            id
        );
    }

    fn write_src_label(&self, target: u32, slot: u32, out: &mut String) {
        use std::fmt::Write as _;
        out.clear();
        let _ = write!(
            out,
            "//{}:{}",
            self.package_path(self.targets[target as usize].package),
            SrcName(target, slot)
        );
    }

    /// As [`SynthGraph::target_hash`], reusing a scratch buffer. Label
    /// formatting dominates hashing at scale, so the buffer matters.
    fn target_hash_into(&self, id: u32, scratch: &mut String) -> [u8; 32] {
        let t = &self.targets[id as usize];
        let mut h = blake3::Hasher::new();
        self.write_target_label(id, scratch);
        h.update(scratch.as_bytes());
        h.update(t.kind.rule_class().as_bytes());
        h.update(&t.attr_salt.to_le_bytes());
        h.update(&[t.tags]);
        // Dead deps are filtered out of the emitted rule inputs, so they must
        // not contribute to the hash either: deleting a target has to move its
        // consumers' hashes.
        for &d in &t.deps {
            if self.targets[d as usize].alive {
                self.write_target_label(d, scratch);
                h.update(scratch.as_bytes());
            }
        }
        // Slots are kept sorted, so hashing them in order is order-independent
        // with respect to the order files were added.
        let mut slots: Vec<u32> = t.srcs.clone();
        slots.sort_unstable();
        for s in slots {
            self.write_src_label(id, s, scratch);
            h.update(scratch.as_bytes());
        }
        for &(_, ft, fs) in &t.foreign_srcs {
            self.write_src_label(ft, fs, scratch);
            h.update(scratch.as_bytes());
        }
        for k in 0..t.ext_deps {
            h.update(Self::ext_label(id, k).as_bytes());
        }
        *h.finalize().as_bytes()
    }

    /// Hash of the package's `.bzl` load closure (spec 5.4).
    pub fn bzl_closure_hash(&self, pkg: u32) -> [u8; 32] {
        let p = &self.packages[pkg as usize];
        let mut entries: Vec<(&str, u32)> = p
            .bzl
            .iter()
            .map(|&b| {
                (
                    self.bzl[b as usize].path.as_str(),
                    self.bzl[b as usize].version,
                )
            })
            .collect();
        entries.sort();
        let mut h = blake3::Hasher::new();
        for (path, ver) in entries {
            h.update(path.as_bytes());
            h.update(&ver.to_le_bytes());
        }
        *h.finalize().as_bytes()
    }

    // ---- conversion to index rows ----

    /// Rows for one package, in the form the index builder consumes.
    pub fn package_update(&self, pkg: u32) -> PackageUpdate {
        self.package_update_with(pkg, &mut String::new())
    }

    /// As [`SynthGraph::package_update`], reusing a scratch buffer for labels.
    fn package_update_with(&self, pkg: u32, scratch: &mut String) -> PackageUpdate {
        let p = &self.packages[pkg as usize];
        let mut targets = Vec::with_capacity(p.targets.len() * 2);
        let mut build = blake3::Hasher::new();
        build.update(p.path.as_bytes());
        for &b in &p.bzl {
            build.update(self.bzl[b as usize].path.as_bytes());
        }
        for &(t, s) in &p.exported {
            build.update(Self::src_name(t, s).as_bytes());
        }
        for &tid in &p.targets {
            let t = &self.targets[tid as usize];
            if !t.alive {
                continue;
            }
            let th = self.target_hash_into(tid, scratch);
            build.update(&th);
            let mut inputs: Vec<String> = Vec::with_capacity(
                t.deps.len() + t.srcs.len() + t.foreign_srcs.len() + t.ext_deps as usize,
            );
            for &d in &t.deps {
                if self.targets[d as usize].alive {
                    self.write_target_label(d, scratch);
                    inputs.push(scratch.clone());
                }
            }
            for &s in &t.srcs {
                self.write_src_label(tid, s, scratch);
                inputs.push(scratch.clone());
            }
            for &(_, ft, fs) in &t.foreign_srcs {
                self.write_src_label(ft, fs, scratch);
                inputs.push(scratch.clone());
            }
            for k in 0..t.ext_deps {
                inputs.push(Self::ext_label(tid, k));
            }
            inputs.sort();
            inputs.dedup();
            self.write_target_label(tid, scratch);
            targets.push(TargetRow {
                label: scratch.clone(),
                kind: NodeKind::Rule,
                rule_class: Some(t.kind.rule_class().to_string()),
                rule_hash: th,
                is_test: t.kind.is_test(),
                manual: t.is_manual(),
                inputs,
                generating_rule: None,
            });
            for &s in &t.srcs {
                self.write_src_label(tid, s, scratch);
                targets.push(TargetRow::source_file(scratch.clone()));
            }
        }
        PackageUpdate {
            package: p.path.clone(),
            build_hash: *build.finalize().as_bytes(),
            bzl_hash: self.bzl_closure_hash(pkg),
            targets,
        }
    }

    /// Rows for every live package, in package order.
    pub fn package_updates(&self) -> Vec<PackageUpdate> {
        use rayon::prelude::*;
        (0..self.packages.len() as u32)
            .into_par_iter()
            .filter(|&p| self.packages[p as usize].alive)
            .map_init(String::new, |scratch, p| {
                self.package_update_with(p, scratch)
            })
            .collect()
    }

    /// Builds a complete index from the graph, bypassing Bazel entirely.
    pub fn build_index(&self, commit: &str) -> cambium_index::IndexData {
        let mut b = cambium_index::IndexBuilder::new(cambium_index::new_lineage());
        b.set_bazel_version("synth");
        b.apply_packages(&self.package_updates());
        // The same static `.bzl` load closure a real cold build computes,
        // scanned out of the load lines `emit_workspace` would have written.
        // Without it every `.bzl` edit would be a global invalidation, which
        // is sound but would make every bzl-edit benchmark and property test
        // vacuous.
        b.set_bzl_closure(self.bzl_closure());
        b.finish(commit)
    }

    /// The `.bzl` load closure of the whole graph.
    pub fn bzl_closure(&self) -> cambium_index::BzlClosure {
        let files = crate::emit::SynthFiles(self);
        let mapping = cambium_index::RepoMapping::identity();
        let mut c = cambium_index::BzlClosure::default();
        for (i, p) in self.packages.iter().enumerate() {
            if !p.alive {
                continue;
            }
            let pc = c.graph.package_closure(&p.path, &files, &mapping);
            debug_assert_eq!(self.pkg_by_path.get(&p.path), Some(&(i as u32)));
            c.packages.insert(p.path.clone(), pc.bzl);
            if !pc.external.is_empty() {
                c.package_external.insert(p.path.clone(), pc.external);
            }
        }
        c.workspace = c.graph.workspace_closure(&files, &mapping);
        c
    }
}

/// Generates a graph from a spec.
///
/// Six phases, each drawing from the one seeded generator in this order, so
/// the output for a preset and a seed is a function of nothing else: the
/// packages and their paths, the `.bzl` files and who loads them, the targets
/// and their layers, the dependency edges, the `test_suite`s, and the
/// `exports_files` relationships.
pub fn generate(spec: &GraphSpec) -> SynthGraph {
    let mut rng = ChaCha8Rng::seed_from_u64(spec.seed);
    let (mut packages, pkg_by_path) = gen_packages(spec, &mut rng);
    let bzl = gen_bzl(spec, &mut rng, &mut packages);
    let layout = gen_targets(spec, &mut rng, &mut packages);
    let mut targets = layout.targets;
    gen_deps(
        spec,
        &mut rng,
        &mut targets,
        &layout.layer_members,
        &layout.pkg_layer_members,
    );
    gen_test_suites(spec, &mut packages, &mut targets);
    gen_exports(spec, &mut packages, &mut targets);

    let mut g = SynthGraph {
        packages,
        targets,
        bzl,
        pkg_by_path,
        bzl_by_path: FxHashMap::default(),
    };
    g.rebuild_pkg_index();
    g
}

/// The packages: as many as the spec names, or enough to hold the targets at
/// the mean package size, with paths whose ancestor segments are shared.
fn gen_packages(
    spec: &GraphSpec,
    rng: &mut ChaCha8Rng,
) -> (Vec<SynthPackage>, FxHashMap<String, u32>) {
    let per_pkg = spec.targets_per_package.mean_hint().max(1.0);
    let n_pkgs = if spec.packages > 0 {
        spec.packages
    } else {
        ((spec.targets as f64 / per_pkg).ceil() as u64).max(1)
    };

    let mut packages: Vec<SynthPackage> = Vec::with_capacity(n_pkgs as usize);
    let mut pkg_by_path = FxHashMap::default();
    for i in 0..n_pkgs {
        let depth = spec.path_depth.sample(rng).max(1);
        let mut path = String::new();
        // Ancestor segments are shared between packages (they come from the
        // high digits of the index), the leaf segment keeps paths unique.
        let mut div = 1u64;
        for _ in 1..depth {
            div = div.saturating_mul(16);
            if path.is_empty() {
                path.push_str(&format!("d{}", i / div));
            } else {
                path.push_str(&format!("/d{}", i / div));
            }
        }
        if path.is_empty() {
            path = format!("p{i}");
        } else {
            path.push_str(&format!("/p{i}"));
        }
        pkg_by_path.insert(path.clone(), i as u32);
        packages.push(SynthPackage {
            path,
            targets: Vec::new(),
            bzl: Vec::new(),
            exported: Vec::new(),
            alive: true,
        });
    }
    (packages, pkg_by_path)
}

/// The `.bzl` files, and which packages load which.
fn gen_bzl(spec: &GraphSpec, rng: &mut ChaCha8Rng, packages: &mut [SynthPackage]) -> Vec<SynthBzl> {
    let mut bzl: Vec<SynthBzl> = (0..spec.bzl_files)
        .map(|i| SynthBzl {
            path: format!("tools/synth/rules{i}.bzl"),
            version: 0,
            loaded_by: Vec::new(),
        })
        .collect();
    if !bzl.is_empty() {
        for (pi, p) in packages.iter_mut().enumerate() {
            let k = spec
                .bzl_loads_per_package
                .sample(rng)
                .min(spec.bzl_files as u64) as usize;
            let mut chosen: Vec<u32> = Vec::with_capacity(k);
            for _ in 0..k {
                let b = rng.gen_range(0..spec.bzl_files);
                if !chosen.contains(&b) {
                    chosen.push(b);
                    bzl[b as usize].loaded_by.push(pi as u32);
                }
            }
            chosen.sort_unstable();
            p.bzl = chosen;
        }
    }
    bzl
}

/// The targets with their layers, plus the two layer indexes the dependency
/// phase draws from.
struct TargetLayout {
    targets: Vec<SynthTarget>,
    /// Target ids per layer.
    layer_members: Vec<Vec<u32>>,
    /// Per package, target ids per layer.
    pkg_layer_members: Vec<FxHashMap<u32, Vec<u32>>>,
}

/// The targets: packages filled round-robin at log-normal sizes, each package
/// at a skewed base layer with a little jitter, kinds drawn per target.
fn gen_targets(
    spec: &GraphSpec,
    rng: &mut ChaCha8Rng,
    packages: &mut [SynthPackage],
) -> TargetLayout {
    let layers = spec.layers.max(1);
    let mut targets: Vec<SynthTarget> = Vec::with_capacity(spec.targets as usize);
    let mut layer_members: Vec<Vec<u32>> = vec![Vec::new(); layers as usize];
    let mut pkg_layer_members: Vec<FxHashMap<u32, Vec<u32>>> =
        vec![FxHashMap::default(); packages.len()];

    let mut next = 0u64;
    let mut pkg_cursor = 0usize;
    while next < spec.targets {
        let pkg = pkg_cursor % packages.len();
        pkg_cursor += 1;
        let size = spec
            .targets_per_package
            .sample(rng)
            .min(spec.targets - next)
            .max(1);
        // Package base layer, skewed.
        let u: f64 = rng.gen();
        let base = ((u.powf(1.0 / spec.layer_skew.max(1e-6)) * layers as f64) as u32)
            .min(layers.saturating_sub(1));
        for _ in 0..size {
            let id = targets.len() as u32;
            let jitter = rng.gen_range(0..3u32);
            let layer = (base + jitter).min(layers - 1);
            let kind = if rng.gen::<f64>() < spec.alias_ratio && layer > 0 {
                Kind::Alias
            } else if rng.gen::<f64>() < spec.test_fraction && layer > 0 {
                Kind::Test
            } else if rng.gen::<f64>() < 0.03 {
                Kind::Bin
            } else {
                Kind::Lib
            };
            // An `alias` rule has exactly one attribute that matters, so it
            // carries no sources; keeping the in-memory model in step with what
            // the Bazel emitter can express is what makes the two comparable.
            let n_srcs = if kind == Kind::Alias {
                0
            } else {
                spec.srcs_per_target.sample(rng) as u32
            };
            targets.push(SynthTarget {
                package: pkg as u32,
                kind,
                layer,
                deps: Vec::new(),
                select_deps: 0,
                ext_deps: 0,
                srcs: (0..n_srcs).collect(),
                src_versions: vec![0; n_srcs as usize],
                foreign_srcs: Vec::new(),
                attr_salt: 0,
                tags: if rng.gen::<f64>() < 0.02 {
                    TAG_MANUAL
                } else {
                    0
                },
                alive: true,
            });
            packages[pkg].targets.push(id);
            layer_members[layer as usize].push(id);
            pkg_layer_members[pkg].entry(layer).or_default().push(id);
            next += 1;
        }
    }
    TargetLayout {
        targets,
        layer_members,
        pkg_layer_members,
    }
}

/// The dependency edges: preferential attachment towards what already has
/// in-edges, same-package edges at the configured ratio, a test on something
/// in its own package, hubs at layer 0, and the `select`/external counts.
fn gen_deps(
    spec: &GraphSpec,
    rng: &mut ChaCha8Rng,
    targets: &mut [SynthTarget],
    layer_members: &[Vec<u32>],
    pkg_layer_members: &[FxHashMap<u32, Vec<u32>>],
) {
    let layers = spec.layers.max(1);
    // Hubs live at layer 0 so everything may depend on them.
    let mut hubs: Vec<u32> = Vec::new();
    if !layer_members[0].is_empty() {
        for i in 0..spec.hub_count.min(layer_members[0].len() as u32) {
            hubs.push(layer_members[0][i as usize]);
        }
    }

    // Prefix sums over lower layers, so a dep can be drawn uniformly from
    // "everything below me" in O(1).
    let mut lower_offsets: Vec<usize> = Vec::with_capacity(layers as usize + 1);
    let mut flat_by_layer: Vec<u32> = Vec::with_capacity(targets.len());
    lower_offsets.push(0);
    for members in layer_members.iter().take(layers as usize) {
        flat_by_layer.extend_from_slice(members);
        lower_offsets.push(flat_by_layer.len());
    }

    // Preferential attachment reservoir: ids appear once per in-edge received.
    const PA_CAP: usize = 4 << 20;
    let mut reservoir: Vec<u32> = Vec::new();
    let mut seen_edges: u64 = 0;

    for id in 0..targets.len() as u32 {
        let layer = targets[id as usize].layer;
        if layer == 0 {
            continue;
        }
        let pool_end = lower_offsets[layer as usize];
        if pool_end == 0 {
            continue;
        }
        let pkg = targets[id as usize].package as usize;
        let want = spec.deps_per_target.sample(rng) as usize;
        let mut deps: Vec<u32> = Vec::with_capacity(want + 2);

        // Alias rules have exactly one dep.
        let want = if targets[id as usize].kind == Kind::Alias {
            1
        } else {
            want
        };

        for _ in 0..want {
            let pick = if rng.gen::<f64>() < spec.preferential_attachment && !reservoir.is_empty() {
                let c = reservoir[rng.gen_range(0..reservoir.len())];
                // Only accept if it keeps the DAG property.
                if targets[c as usize].layer < layer {
                    Some(c)
                } else {
                    None
                }
            } else {
                None
            };
            let pick = pick.or_else(|| {
                if rng.gen::<f64>() >= spec.cross_package_dep_ratio {
                    // Same package, strictly lower layer.
                    let mut cands: Vec<u32> = Vec::new();
                    for l in 0..layer {
                        if let Some(v) = pkg_layer_members[pkg].get(&l) {
                            cands.extend_from_slice(v);
                        }
                    }
                    if !cands.is_empty() {
                        return Some(cands[rng.gen_range(0..cands.len())]);
                    }
                }
                Some(flat_by_layer[rng.gen_range(0..pool_end)])
            });
            if let Some(d) = pick {
                if d != id && !deps.contains(&d) {
                    deps.push(d);
                }
            }
        }

        // A test depends on something in its own package where possible.
        if targets[id as usize].kind == Kind::Test {
            let mut cands: Vec<u32> = Vec::new();
            for l in 0..layer {
                if let Some(v) = pkg_layer_members[pkg].get(&l) {
                    cands.extend(
                        v.iter()
                            .copied()
                            .filter(|&c| matches!(targets[c as usize].kind, Kind::Lib | Kind::Bin)),
                    );
                }
            }
            if !cands.is_empty() {
                let d = cands[rng.gen_range(0..cands.len())];
                if !deps.contains(&d) {
                    deps.push(d);
                }
            }
        }

        // Hub attachment.
        if !hubs.is_empty() && rng.gen::<f64>() < spec.hub_fraction {
            let h = hubs[rng.gen_range(0..hubs.len())];
            if h != id && targets[h as usize].layer < layer && !deps.contains(&h) {
                deps.push(h);
            }
        }

        for &d in &deps {
            seen_edges += 1;
            if reservoir.len() < PA_CAP {
                reservoir.push(d);
            } else {
                // Reservoir replacement keeps the hub skew without unbounded memory.
                let slot = (rng.gen::<u64>() % seen_edges) as usize;
                if slot < PA_CAP {
                    reservoir[slot] = d;
                }
            }
        }

        let n_select = if spec.select_ratio > 0.0 && !deps.is_empty() {
            let n = (deps.len() as f64 * spec.select_ratio).round() as u16;
            n.min(deps.len() as u16)
        } else {
            0
        };
        let ext = if targets[id as usize].kind != Kind::Alias
            && rng.gen::<f64>() < spec.external_dep_ratio
        {
            1 + (rng.gen::<u16>() % 2)
        } else {
            0
        };
        let t = &mut targets[id as usize];
        t.deps = deps;
        t.select_deps = n_select;
        t.ext_deps = ext;
    }
}

/// The `test_suite` targets: one per chosen package, collecting its tests,
/// one layer above them. Deterministic without the generator: the packages
/// are chosen by stride.
fn gen_test_suites(
    spec: &GraphSpec,
    packages: &mut [SynthPackage],
    targets: &mut Vec<SynthTarget>,
) {
    let n_suites = (packages.len() as f64 * spec.test_suite_fraction) as usize;
    for i in 0..n_suites {
        let pkg = (i * 7 + 1) % packages.len();
        let tests: Vec<u32> = packages[pkg]
            .targets
            .iter()
            .copied()
            .filter(|&t| targets[t as usize].kind == Kind::Test)
            .collect();
        if tests.is_empty() {
            continue;
        }
        let layer = tests
            .iter()
            .map(|&t| targets[t as usize].layer)
            .max()
            .unwrap_or(0)
            + 1;
        let id = targets.len() as u32;
        targets.push(SynthTarget {
            package: pkg as u32,
            kind: Kind::TestSuite,
            // One above the tests it collects, which may be one above the
            // top layer; nothing is generated to depend on a suite.
            layer,
            deps: tests,
            select_deps: 0,
            ext_deps: 0,
            srcs: Vec::new(),
            src_versions: Vec::new(),
            foreign_srcs: Vec::new(),
            attr_salt: 0,
            tags: 0,
            alive: true,
        });
        packages[pkg].targets.push(id);
    }
}

/// The `exports_files` relationships: a package exports one of a target's
/// sources and a target in another package consumes it. Chosen by stride,
/// like the suites.
fn gen_exports(spec: &GraphSpec, packages: &mut [SynthPackage], targets: &mut [SynthTarget]) {
    let n_exports = (packages.len() as f64 * spec.exports_files_ratio) as usize;
    for i in 0..n_exports {
        let exporter = (i * 13 + 3) % packages.len();
        let Some(&et) = packages[exporter]
            .targets
            .iter()
            .find(|&&t| !targets[t as usize].srcs.is_empty())
        else {
            continue;
        };
        let slot = targets[et as usize].srcs[0];
        let consumer_pkg = (exporter + 1 + i * 5) % packages.len();
        if consumer_pkg == exporter {
            continue;
        }
        // Aliases and test_suites have no srcs attribute to hang the file on.
        let Some(&ct) = packages[consumer_pkg]
            .targets
            .iter()
            .find(|&&t| matches!(targets[t as usize].kind, Kind::Lib | Kind::Bin | Kind::Test))
        else {
            continue;
        };
        packages[exporter].exported.push((et, slot));
        targets[ct as usize]
            .foreign_srcs
            .push((exporter as u32, et, slot));
    }
}

impl SynthGraph {
    pub(crate) fn rebuild_pkg_index(&mut self) {
        self.pkg_by_path.clear();
        for (i, p) in self.packages.iter().enumerate() {
            if p.alive {
                self.pkg_by_path.insert(p.path.clone(), i as u32);
            }
        }
        self.bzl_by_path.clear();
        for (i, b) in self.bzl.iter().enumerate() {
            self.bzl_by_path.insert(b.path.clone(), i as u32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_deterministic() {
        let spec = GraphSpec::preset("small").unwrap();
        let a = generate(&spec);
        let b = generate(&spec);
        assert_eq!(a.targets.len(), b.targets.len());
        for i in 0..a.targets.len() {
            assert_eq!(a.targets[i].deps, b.targets[i].deps);
            assert_eq!(a.target_hash(i as u32), b.target_hash(i as u32));
        }
        assert_eq!(
            a.build_index("x").header.counts.edges,
            b.build_index("x").header.counts.edges
        );
    }

    /// The generator's output is pinned to a hash: a preset and a seed must
    /// produce the same workspace on every machine and after every refactor,
    /// because a synthetic subject is emitted on demand and compared with
    /// itself across runs. Everything the emitter writes is folded in — the
    /// package paths and their `BUILD` text, the `.bzl` files, every target's
    /// hash — plus five commits of the default change preset, so a change to
    /// the order of a single random draw moves the digest.
    #[test]
    fn a_small_workspace_and_its_history_hash_to_what_they_did() {
        let mut g = generate(&GraphSpec::preset("small").unwrap());
        let mut h = blake3::Hasher::new();
        for (i, p) in g.packages.iter().enumerate() {
            h.update(p.path.as_bytes());
            h.update(crate::emit::build_file_loads(&g, i as u32).as_bytes());
        }
        for b in &g.bzl {
            h.update(b.path.as_bytes());
            h.update(&b.version.to_le_bytes());
        }
        for id in 0..g.targets.len() as u32 {
            h.update(&g.target_hash(id));
        }
        let before = h.finalize().to_hex().to_string();
        assert_eq!(
            before, "4bed7f55d5e486941136674f64cdd6c81955515dd95465a7638867c4eea0d42b",
            "the generated graph moved"
        );
        let spec = crate::ChangeSpec::default();
        let mut h = blake3::Hasher::new();
        for rev in 0..5 {
            for c in crate::commit(&mut g, &spec, rev).changes {
                h.update(format!("{:?} {}\n", c.status, c.path).as_bytes());
            }
        }
        assert_eq!(
            h.finalize().to_hex().to_string(),
            "14bcc52f9b03050bb78ede0d672f64291ef4eaa6f86b69011b9b44326c241755",
            "the generated history moved"
        );
    }

    #[test]
    fn graph_is_acyclic_by_layer() {
        let g = generate(&GraphSpec::preset("small").unwrap());
        for (i, t) in g.targets.iter().enumerate() {
            for &d in &t.deps {
                assert!(
                    g.targets[d as usize].layer < t.layer,
                    "target {i} at layer {} depends on layer {}",
                    t.layer,
                    g.targets[d as usize].layer
                );
            }
        }
    }

    #[test]
    fn produces_a_usable_index() {
        let g = generate(&GraphSpec::preset("small").unwrap());
        let d = g.build_index("commit0");
        let v = d.view();
        assert!(v.num_nodes() > 2_000);
        assert_eq!(v.num_packages() as usize, g.packages.len());
        assert!(v.counts().tests > 0);
        // Every dep of every target is an edge in the index.
        let m = v.labels.build_map();
        for id in 0..g.targets.len() as u32 {
            let src = m[g.target_label(id).as_str()];
            for &d in &g.targets[id as usize].deps {
                let dst = m[g.target_label(d).as_str()];
                assert!(v.forward.row(src).contains(&dst));
            }
        }
    }
}
