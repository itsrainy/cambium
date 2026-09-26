//! Index construction and splicing.
//!
//! One code path serves both cases: a cold full build is a
//! splice of every package into an empty index, and an incremental update is a
//! splice of the dirty packages into a loaded one. The difference is only how
//! many rows get rebuilt.

use std::collections::{BTreeMap, BTreeSet};

use fixedbitset::FixedBitSet;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::csr::{sort_dedup_pairs, Csr};
use crate::data::{
    flags, pkg_flags, Counts, Header, IndexData, IndexRef, NodeKind, Provenance, NO_ID,
};
use crate::overlay::Overlay;
use crate::space::{LabelSpace, NodeMeta, NodeSpace};
use crate::strtab::Interner;

/// A target as parsed from `bazel query` output (or synthesised in tests).
#[derive(Clone, Debug)]
pub struct TargetRow {
    pub label: String,
    pub kind: NodeKind,
    /// Rule class name; `None` for non-rules.
    pub rule_class: Option<String>,
    /// blake3 of the canonicalised rule proto; zero for non-rules.
    pub rule_hash: [u8; 32],
    pub is_test: bool,
    pub manual: bool,
    /// `rule_input` labels. Rules only.
    pub inputs: Vec<String>,
    /// Generating rule label. `GeneratedFile` only.
    pub generating_rule: Option<String>,
}

impl TargetRow {
    pub fn source_file(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            kind: NodeKind::SourceFile,
            rule_class: None,
            rule_hash: [0; 32],
            is_test: false,
            manual: false,
            inputs: Vec::new(),
            generating_rule: None,
        }
    }

    pub fn rule(label: impl Into<String>, class: impl Into<String>, inputs: Vec<String>) -> Self {
        let class = class.into();
        let is_test = class.ends_with("_test") || class == "test_suite";
        Self {
            label: label.into(),
            kind: NodeKind::Rule,
            rule_class: Some(class),
            rule_hash: [0; 32],
            is_test,
            manual: false,
            inputs,
            generating_rule: None,
        }
    }
}

/// Everything a splice needs to know about one package.
#[derive(Clone, Debug, Default)]
pub struct PackageUpdate {
    pub package: String,
    pub build_hash: [u8; 32],
    /// Hash of the package's transitive `.bzl` load closure. Reserved: the
    /// closure itself is stored (see [`BzlClosure`]), which answers the
    /// question a hash cannot — *which* packages a changed `.bzl` reaches — so
    /// this is written as zero unless a caller supplies it.
    pub bzl_hash: [u8; 32],
    pub targets: Vec<TargetRow>,
}

/// What a splice changed in one package, for direct-set derivation.
#[derive(Clone, Debug, Default)]
pub struct PackageDelta {
    pub package_id: u32,
    pub package: String,
    /// Ids that are new or whose rule hash changed.
    pub changed: Vec<u32>,
    /// Ids that disappeared from the package and were tombstoned.
    pub deleted: Vec<u32>,
    /// Ids present in the package after the splice.
    pub current: Vec<u32>,
}

/// Result of applying a batch of package updates.
#[derive(Clone, Debug, Default)]
pub struct SpliceReport {
    pub packages: Vec<PackageDelta>,
}

impl SpliceReport {
    pub fn changed_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.packages.iter().flat_map(|p| p.changed.iter().copied())
    }

    pub fn deleted_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.packages.iter().flat_map(|p| p.deleted.iter().copied())
    }
}

/// The static `.bzl` load closure, in the mutable form a build or a splice
/// works with.
///
/// Ids are the [`crate::loads::LoadGraph`]'s, and [`IndexBuilder::finish`]
/// re-sorts them into path order on the way out so the serialised table can be
/// binary-searched.
#[derive(Clone, Debug, Default)]
pub struct BzlClosure {
    /// The load graph: paths, direct loads, external labels.
    pub graph: crate::loads::LoadGraph,
    /// Package name -> the `.bzl` ids its BUILD file transitively loads.
    pub packages: std::collections::BTreeMap<String, Vec<u32>>,
    /// Package name -> the canonical external labels its BUILD file loads
    /// *directly*. Only packages with at least one have an entry. See
    /// [`crate::loads::PackageClosure`].
    pub package_external: std::collections::BTreeMap<String, Vec<String>>,
    /// `.bzl` ids reachable from `MODULE.bazel` / `WORKSPACE`.
    pub workspace: Vec<u32>,
    /// Which main-repo files feed which external repositories, and which
    /// repositories each workspace-closure `.bzl` defines. Carried alongside
    /// the closure because it is computed from the same walk over the same
    /// files; see [`crate::externals`].
    pub inputs: crate::externals::RepoInputs,
}

impl BzlClosure {
    /// Reads a closure back out of an index, so a splice can update the
    /// packages it re-queried and carry the rest unchanged.
    pub fn from_view(v: IndexRef<'_>) -> Option<Self> {
        if !v.bzl_closure {
            return None;
        }
        let mut packages: std::collections::BTreeMap<String, Vec<u32>> = Default::default();
        for id in 0..v.num_bzl() {
            for &p in v.bzl_consumers(id) {
                packages
                    .entry(v.package_name(p).to_string())
                    .or_default()
                    .push(id);
            }
        }
        for ids in packages.values_mut() {
            ids.sort_unstable();
        }
        let mut package_external: std::collections::BTreeMap<String, Vec<String>> =
            Default::default();
        for p in 0..v.num_packages() {
            let ext = v.package_external(p);
            if !ext.is_empty() {
                package_external.insert(
                    v.package_name(p).to_string(),
                    ext.into_iter().map(str::to_string).collect(),
                );
            }
        }
        let mut inputs = crate::externals::RepoInputs::default();
        for r in 0..v.num_repos() {
            let name = v.repo_name(r).to_string();
            inputs
                .canonical
                .insert(name.clone(), v.repo_canonical.get(r).to_string());
            // The mapping matters as much as the repository does: without it
            // the engine cannot tell an input-path edit from an edit to a
            // `.bzl` the repository exports, and falls back to re-querying
            // every package that loads one. A repository that genuinely has
            // no derivable mapping keeps its empty entry out of the map, so
            // the two cases stay distinguishable.
            let maps = v.repo_path_maps(r);
            if !maps.is_empty() {
                inputs
                    .path_maps
                    .insert(name.clone(), maps.into_iter().collect());
            }
            // The origin has to survive a splice for the same reason the
            // path mapping does: a base index carries what the last cold
            // build derived, and a run that dropped it would treat every
            // repository as declared-only.
            let o = v.repo_origin(r);
            if o != crate::externals::RepoOrigin::default() {
                inputs.origins.insert(name.clone(), o);
            }
            // And the consumers, which only a cold build's Bazel can say and
            // every splice has to carry.
            let consumers = v.repo_consumers(r);
            if !consumers.is_empty() {
                inputs
                    .consumers
                    .insert(name.clone(), consumers.into_iter().collect());
            }
            inputs.repos.insert(name);
        }
        for p in 0..v.repo_input_patterns.len() as u32 {
            let pat = crate::externals::Pattern::from_stored(
                v.repo_input_patterns.get(p),
                v.repo_input_prefix[p as usize],
            );
            if v.repo_input_target_only[p as usize] {
                inputs.target_only.insert(pat.clone());
            }
            let set = inputs.patterns.entry(pat).or_default();
            for &r in v.repo_inputs.row(p) {
                set.insert(v.repo_name(r).to_string());
            }
        }
        for id in 0..v.num_bzl() {
            let row = v.bzl_repos_of(id);
            if row.is_empty() {
                continue;
            }
            inputs.bzl_repos.insert(
                v.bzl_path(id).to_string(),
                row.iter().map(|&r| v.repo_name(r).to_string()).collect(),
            );
        }
        Some(Self {
            graph: crate::loads::LoadGraph::from_index(v),
            packages,
            package_external,
            workspace: v.bzl_workspace.to_vec(),
            inputs,
        })
    }
}

/// Mutable index. Built empty for a full build, or over a loaded index for a
/// splice.
///
/// A splice *borrows* its base rather than copying it: the label table and node
/// columns of a large index are hundreds of megabytes, and copying them is the
/// single most expensive step in a pull-request run. Which of the two
/// finalizers you call decides whether that copy happens at all —
/// [`IndexBuilder::finish`] materialises a complete new index, and
/// [`IndexBuilder::finish_overlay`] produces only the delta.
pub struct IndexBuilder<'a> {
    base: Option<IndexRef<'a>>,
    labels: LabelSpace<'a>,
    nodes: NodeSpace<'a>,

    // Small tables: copied even for a splice, because they are thousands of
    // entries rather than millions and the copy is not measurable.
    classes: Interner,
    pkg_names: Interner,
    pkg_nodes: Vec<Vec<u32>>,
    pkg_bzl_hash: Vec<[u8; 32]>,
    pkg_build_hash: Vec<[u8; 32]>,
    pkg_flags: Vec<u8>,

    /// Rows replaced by this generation.
    rebuilt: FixedBitSet,
    /// Staged `(src, dst)` edges for rebuilt rows.
    pairs: Vec<(u32, u32)>,

    /// The `.bzl` closure this generation will write, once known. For a splice
    /// it starts out unread (`bzl_unread`): reading it back out of the base is
    /// O(closure), and only [`IndexBuilder::finish`] or a caller updating it
    /// needs it — `affected` never does.
    bzl: Option<BzlClosure>,
    bzl_unread: bool,

    /// The Skyframe loading layer this generation will write, once known.
    /// Carried the same lazy way as the `.bzl` closure above, and for the same
    /// reason: inverting the stored reverse edges is O(edges), and only a run
    /// that materialises an index pays for it.
    sky: Option<crate::sky::SkyLayer>,
    sky_unread: bool,

    /// Directories of the checkout that are Bazel repositories of their own
    /// (`cambium_index::repo_boundaries`), each with whether `.bazelignore`
    /// names it. Carried across a splice from the base, and refreshed by
    /// whoever has a checkout to walk.
    boundaries: Vec<crate::Boundary>,

    /// What the leaves' round-trip rows were derived through, sorted and
    /// deduplicated ([`IndexData::round_trip_via_bytes`]). Carried across a
    /// splice from the base, and replaced or widened by whoever asks Bazel
    /// for rows again.
    round_trip_via: Vec<String>,

    /// Repository (`@name`) -> the leaves whose closure runs through it
    /// ([`IndexData::leaf_closure_repo_bytes`]). Carried across a splice from
    /// the base, a leaf's entries replaced whenever Bazel is asked about it
    /// again ([`IndexBuilder::set_leaf_closure`]).
    leaf_closure: BTreeMap<String, BTreeSet<u32>>,

    lineage: [u8; 16],
    bazel_version: String,
    config_hash: [u8; 32],
    partial: bool,
    packages_in_error: u32,
    error_packages: Vec<String>,
}

impl<'a> IndexBuilder<'a> {
    /// Empty index for a cold build. `lineage` should be freshly minted.
    pub fn new(lineage: [u8; 16]) -> Self {
        Self {
            base: None,
            labels: LabelSpace::empty(),
            nodes: NodeSpace::empty(),
            classes: Interner::new(),
            pkg_names: Interner::new(),
            pkg_nodes: Vec::new(),
            pkg_bzl_hash: Vec::new(),
            pkg_build_hash: Vec::new(),
            pkg_flags: Vec::new(),
            rebuilt: FixedBitSet::with_capacity(0),
            pairs: Vec::new(),
            bzl: None,
            bzl_unread: false,
            sky: None,
            sky_unread: false,
            boundaries: Vec::new(),
            round_trip_via: Vec::new(),
            leaf_closure: BTreeMap::new(),
            lineage,
            bazel_version: String::new(),
            config_hash: [0; 32],
            partial: false,
            packages_in_error: 0,
            error_packages: Vec::new(),
        }
    }

    /// Opens a base index for splicing, preserving every id and the lineage.
    ///
    /// Nothing large is copied here. Labels and node columns are read from
    /// `v` through [`LabelSpace`] and [`NodeSpace`], which record only what the
    /// splice appends or overrides; the base forward rows are consulted when a
    /// finalizer needs them. Only the small per-package and per-class tables
    /// are copied, because they are thousands of entries rather than millions.
    ///
    /// Which finalizer is called decides what the splice costs in the end:
    ///
    /// - [`IndexBuilder::finish`] materialises a complete new index — labels,
    ///   columns, both CSRs — for `baseline update`, whose output is published
    ///   and spliced onto next time. The property that a spliced index equals
    ///   a full rebuild is what the correctness oracles check on every commit.
    /// - [`IndexBuilder::finish_overlay`] produces only the delta, for
    ///   `affected`, which needs to walk the spliced graph once and never
    ///   needs an artifact. It costs the staged rows and the reverse
    ///   corrections they imply, proportional to the change.
    pub fn from_view(v: IndexRef<'a>) -> Self {
        let (cb, co) = (v.classes.bytes.to_vec(), v.classes.offsets.to_vec());
        let (pb, po) = (v.pkg_names.bytes.to_vec(), v.pkg_names.offsets.to_vec());
        let n = v.num_nodes() as usize;
        let np = v.num_packages() as usize;
        let mut pkg_nodes = Vec::with_capacity(np);
        for p in 0..np as u32 {
            pkg_nodes.push(v.package_nodes(p).to_vec());
        }
        Self {
            base: Some(v),
            labels: LabelSpace::over(v.labels, v.label_sorted),
            nodes: NodeSpace::over(v),
            classes: Interner::from_parts(cb, co),
            pkg_names: Interner::from_parts(pb, po),
            pkg_nodes,
            pkg_bzl_hash: v.pkg_bzl_hash.to_vec(),
            pkg_build_hash: v.pkg_build_hash.to_vec(),
            pkg_flags: v.pkg_flags.to_vec(),
            rebuilt: FixedBitSet::with_capacity(n),
            pairs: Vec::new(),
            bzl: None,
            bzl_unread: v.bzl_closure,
            sky: None,
            sky_unread: v.sky.present,
            boundaries: v.boundaries().collect(),
            round_trip_via: v.round_trip_via().map(str::to_string).collect(),
            leaf_closure: v
                .leaf_closure()
                .map(|(r, ids)| (r.to_string(), ids.iter().copied().collect()))
                .collect(),
            lineage: v.lineage,
            bazel_version: v.bazel_version.to_string(),
            config_hash: v.config_hash,
            partial: v.partial,
            packages_in_error: v.packages_in_error,
            error_packages: v.error_packages.to_vec(),
        }
    }

    /// Records the repository boundaries of the checkout these rows describe.
    /// Sorted and de-duplicated here so the payload stays a function of the
    /// tree rather than of the walk order.
    pub fn set_repo_boundaries(&mut self, mut dirs: Vec<crate::Boundary>) {
        dirs.sort();
        dirs.dedup_by(|a, b| a.dir == b.dir);
        self.boundaries = dirs;
    }

    /// Records what the leaves' round-trip rows were derived through
    /// ([`IndexData::round_trip_via_bytes`]), replacing what was carried.
    /// Sorted and deduplicated here, like the boundaries.
    pub fn set_round_trip_via(&mut self, via: impl IntoIterator<Item = String>) {
        let mut v: Vec<String> = via.into_iter().collect();
        v.sort();
        v.dedup();
        self.round_trip_via = v;
    }

    /// What this generation will record the round-trip rows as derived
    /// through: the base's, until [`IndexBuilder::set_round_trip_via`].
    pub fn round_trip_via(&self) -> &[String] {
        &self.round_trip_via
    }

    /// Records that the closure of the leaf `id` runs through `repos`
    /// (`@name` entries, [`IndexData::leaf_closure_repo_bytes`]), replacing
    /// what was recorded for it. Empty `repos` forgets the leaf.
    pub fn set_leaf_closure<'r>(&mut self, id: u32, repos: impl IntoIterator<Item = &'r str>) {
        for leaves in self.leaf_closure.values_mut() {
            leaves.remove(&id);
        }
        for r in repos {
            self.leaf_closure
                .entry(r.to_string())
                .or_default()
                .insert(id);
        }
        self.leaf_closure.retain(|_, leaves| !leaves.is_empty());
    }

    pub fn set_bazel_version(&mut self, v: impl Into<String>) {
        self.bazel_version = v.into();
    }

    /// Records that the query behind these rows finished with load errors, so
    /// the index describes a repository minus the packages Bazel could not
    /// load. Carried through splices by [`IndexBuilder::from_view`]: an
    /// incremental update over a partial base is still partial.
    pub fn set_partial(&mut self, packages_in_error: u32, error_packages: Vec<String>) {
        self.partial = true;
        self.packages_in_error = self.packages_in_error.max(packages_in_error);
        if self.error_packages.is_empty() {
            self.error_packages = error_packages;
        }
    }

    /// Installs (or replaces) the static `.bzl` load closure.
    pub fn set_bzl_closure(&mut self, c: BzlClosure) {
        self.bzl = Some(c);
        self.bzl_unread = false;
    }

    /// The closure carried over from the base, for a caller that wants to
    /// update only the packages it re-queried. Read out of the base on first
    /// use; a builder that is only ever asked for an overlay never pays for it.
    pub fn take_bzl_closure(&mut self) -> Option<BzlClosure> {
        self.read_base_bzl();
        self.bzl.take()
    }

    fn read_base_bzl(&mut self) {
        if std::mem::take(&mut self.bzl_unread) {
            self.bzl = self.base.and_then(BzlClosure::from_view);
        }
    }

    /// Installs (or replaces) the Skyframe loading layer.
    pub fn set_sky_layer(&mut self, l: crate::sky::SkyLayer) {
        self.sky = Some(l);
        self.sky_unread = false;
    }

    /// The layer carried over from the base, for a caller that wants to
    /// refresh only the packages it re-queried. Read out of the base on first
    /// use, so a builder that is only ever asked for an overlay never pays the
    /// inversion.
    pub fn take_sky_layer(&mut self) -> Option<crate::sky::SkyLayer> {
        if std::mem::take(&mut self.sky_unread) {
            self.sky = self
                .base
                .and_then(|v| crate::sky::SkyLayer::from_view(v.sky));
        }
        self.sky.take()
    }

    pub fn set_config_hash(&mut self, h: [u8; 32]) {
        self.config_hash = h;
    }

    pub fn lineage(&self) -> [u8; 16] {
        self.lineage
    }

    pub fn num_nodes(&self) -> u32 {
        self.nodes.len()
    }

    pub fn label_id(&self, label: &str) -> Option<u32> {
        self.labels.lookup(label)
    }

    pub fn label(&self, id: u32) -> &str {
        self.labels.get(id)
    }

    /// Whether `id` names a node this index has retired.
    pub fn is_tombstone(&self, id: u32) -> bool {
        self.nodes.get(id).flags & flags::TOMBSTONE != 0
    }

    /// Whether `id` is a leaf: a label some rule names that no package of
    /// this index owns (an `Unknown` node), which for an external label means
    /// a target of a repository the index does not hold.
    pub fn is_leaf(&self, id: u32) -> bool {
        self.nodes.get(id).kind == NodeKind::Unknown as u8
    }

    /// Replaces the forward row of the leaf `id` with edges to `dsts`: the
    /// targets inside the index's universe it reaches through targets outside
    /// it (a main -> external -> main round trip), so the reverse walk from
    /// one of them reaches the leaf and the rules that name it.
    ///
    /// No package owns a leaf, so no splice rebuilds its row: `apply_packages`
    /// replaces the rows of the packages it is given and `delete_package` the
    /// rows of the package it retires, and a leaf is in neither. The row set
    /// here is carried verbatim by every later splice until the next call for
    /// the same leaf replaces it. A label that becomes a package's target
    /// (a repository the universe starts to include) gets that package's row
    /// instead, which is the real one.
    ///
    /// Returns `false`, changing nothing, for a node that is not a leaf.
    pub fn set_leaf_row(&mut self, id: u32, dsts: &[u32]) -> bool {
        if !self.is_leaf(id) {
            return false;
        }
        if self.rebuilt.contains(id as usize) {
            self.pairs.retain(|&(src, _)| src != id);
        }
        self.mark_rebuilt(id);
        self.pairs
            .extend(dsts.iter().filter(|&&d| d != id).map(|&d| (id, d)));
        true
    }

    /// The forward row `id` will have: the one staged this generation for a
    /// row it rebuilt, the base's otherwise.
    pub fn forward_row(&self, id: u32) -> Vec<u32> {
        if self.rebuilt.contains(id as usize) {
            let mut row: Vec<u32> = self
                .pairs
                .iter()
                .filter(|&&(src, _)| src == id)
                .map(|&(_, d)| d)
                .collect();
            row.sort_unstable();
            row.dedup();
            row
        } else {
            self.base_forward_row(id).to_vec()
        }
    }

    /// Forward row of `id` in the base, or empty for a node this generation
    /// created.
    fn base_forward_row(&self, id: u32) -> &[u32] {
        match self.base {
            Some(b) if id < self.nodes.base_len() => b.forward.row(id),
            _ => &[],
        }
    }

    pub fn package_id(&self, package: &str) -> Option<u32> {
        self.pkg_names.lookup(package)
    }

    /// Interns a label, allocating an `Unknown` node if it is new.
    fn node_for(&mut self, label: &str) -> u32 {
        let (id, is_new) = self.labels.intern_new(label);
        if is_new {
            self.nodes.push(NodeMeta::unknown());
        }
        id
    }

    /// Marks a forward row as replaced this generation. Grows the bitset by
    /// doubling: `FixedBitSet::grow` reallocates to exactly the requested size,
    /// so growing by one per node would make a full build quadratic.
    #[inline]
    fn mark_rebuilt(&mut self, id: u32) {
        let need = id as usize + 1;
        if need > self.rebuilt.len() {
            self.rebuilt.grow(need.max(self.rebuilt.len() * 2).max(64));
        }
        self.rebuilt.insert(id as usize);
    }

    fn package_slot(&mut self, name: &str) -> u32 {
        let (id, is_new) = self.pkg_names.intern_new(name);
        if is_new {
            self.pkg_nodes.push(Vec::new());
            self.pkg_bzl_hash.push([0; 32]);
            self.pkg_build_hash.push([0; 32]);
            self.pkg_flags.push(0);
        }
        id
    }

    /// Applies a batch of package updates, replacing every row those packages own.
    ///
    /// Runs in two phases so that a target may reference a label defined later
    /// in the same batch: phase one interns and stamps node metadata, phase two
    /// resolves edges.
    ///
    /// A generated file is an ordinary node with one forward edge, to the rule
    /// that generates it, so a consumer's edge to `//g:out.h` stays pointed at
    /// the file and a change to the generator reaches the consumer in two
    /// hops. The edge `out.h -> gen` is owned by the generator's package, which
    /// is what makes a splice equal a full rebuild: re-querying that package
    /// alone rewrites exactly the row that moved when a different rule starts
    /// producing the file. Redirecting consumers straight at the generator
    /// would be one hop shorter but would bake the *current* generator into
    /// rows owned by packages the splice never touches, and a later splice of
    /// the generator's package alone could not know which carried rows to fix
    /// (a consumer's rebuilt row has no record of whether `gen` was in it
    /// because of `out.h` or in its own right). One more visited node per
    /// consumed generated file is a small price for that.
    pub fn apply_packages(&mut self, updates: &[PackageUpdate]) -> SpliceReport {
        // Phase 1: nodes.
        let mut deltas: Vec<PackageDelta> = Vec::with_capacity(updates.len());
        for up in updates {
            let pkg_id = self.package_slot(&up.package);
            let old_ids: FxHashSet<u32> = self.pkg_nodes[pkg_id as usize].iter().copied().collect();
            let mut current = Vec::with_capacity(up.targets.len());
            let mut changed = Vec::new();

            for t in &up.targets {
                let id = self.node_for(&t.label);
                let old = self.nodes.get(id);
                let was_known =
                    old.kind != NodeKind::Unknown as u8 && old.flags & flags::TOMBSTONE == 0;
                let hash_changed = old.hash != t.rule_hash;

                let class = match &t.rule_class {
                    Some(c) => self.classes.intern(c),
                    None => NO_ID,
                };
                let mut f = old.flags & !(flags::TOMBSTONE | flags::MANUAL);
                if t.manual {
                    f |= flags::MANUAL;
                }
                self.nodes.set(
                    id,
                    NodeMeta {
                        kind: t.kind as u8,
                        package: pkg_id,
                        class,
                        hash: t.rule_hash,
                        is_test: t.is_test,
                        flags: f,
                    },
                );

                self.mark_rebuilt(id);
                current.push(id);
                if t.kind == NodeKind::Rule && (!was_known || hash_changed) {
                    changed.push(id);
                }
            }

            current.sort_unstable();
            current.dedup();
            let new_set: FxHashSet<u32> = current.iter().copied().collect();
            let mut deleted: Vec<u32> = old_ids.difference(&new_set).copied().collect();
            deleted.sort_unstable();
            for &d in &deleted {
                self.tombstone(d);
                self.mark_rebuilt(d); // clears the forward row
            }

            self.pkg_nodes[pkg_id as usize] = current.clone();
            self.pkg_build_hash[pkg_id as usize] = up.build_hash;
            self.pkg_bzl_hash[pkg_id as usize] = up.bzl_hash;
            self.pkg_flags[pkg_id as usize] = if current.is_empty() {
                pkg_flags::TOMBSTONE
            } else {
                0
            };

            deltas.push(PackageDelta {
                package_id: pkg_id,
                package: up.package.clone(),
                changed,
                deleted,
                current,
            });
        }

        // Phase 2: edges.
        for up in updates {
            for t in &up.targets {
                let src = self.labels.lookup(&t.label).expect("interned in phase 1");
                match t.kind {
                    NodeKind::GeneratedFile => {
                        if let Some(gen) = &t.generating_rule {
                            let dst = self.node_for(gen);
                            self.pairs.push((src, dst));
                        }
                    }
                    NodeKind::Rule => {
                        for inp in &t.inputs {
                            let dst = self.node_for(inp);
                            if dst != src {
                                self.pairs.push((src, dst));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        SpliceReport { packages: deltas }
    }

    /// Marks a node as gone: kind, flags and everything derived from its
    /// definition are cleared, but the id is never reused.
    fn tombstone(&mut self, id: u32) {
        let mut m = self.nodes.get(id);
        m.kind = NodeKind::Tombstone as u8;
        m.flags |= flags::TOMBSTONE;
        m.class = NO_ID;
        m.hash = [0; 32];
        m.is_test = false;
        self.nodes.set(id, m);
    }

    /// Marks a package as gone: tombstones everything it owned.
    /// Returns the tombstoned ids.
    pub fn delete_package(&mut self, package: &str) -> Vec<u32> {
        let Some(pkg_id) = self.pkg_names.lookup(package) else {
            return Vec::new();
        };
        let ids = std::mem::take(&mut self.pkg_nodes[pkg_id as usize]);
        for &d in &ids {
            self.tombstone(d);
            self.mark_rebuilt(d);
        }
        self.pkg_flags[pkg_id as usize] = pkg_flags::TOMBSTONE;
        self.pkg_build_hash[pkg_id as usize] = [0; 32];
        self.pkg_bzl_hash[pkg_id as usize] = [0; 32];
        ids
    }

    /// Builds the forward CSR: staged rows for everything rebuilt this
    /// generation, base rows copied verbatim for everything else.
    fn finish_forward(&mut self) -> Csr {
        let n = self.nodes.len() as usize;
        let mut pairs = std::mem::take(&mut self.pairs);
        sort_dedup_pairs(&mut pairs);

        // Row lengths.
        let mut offsets = vec![0u32; n + 1];
        let base = self.base.map(|b| b.forward).unwrap_or(crate::CsrRef::EMPTY);
        let mut i = 0usize;
        while i < pairs.len() {
            let src = pairs[i].0;
            let mut j = i;
            while j < pairs.len() && pairs[j].0 == src {
                j += 1;
            }
            offsets[src as usize + 1] = (j - i) as u32;
            i = j;
        }
        // Written at u + 1 while the row length is read from the base CSR at u,
        // so this iterates the offsets rather than the range.
        for (u, slot) in offsets.iter_mut().enumerate().skip(1) {
            let u = u - 1;
            if !self.rebuilt.contains(u) {
                *slot = base.row_len(u as u32) as u32;
            }
        }
        crate::csr::prefix_sum(&mut offsets);

        let total = offsets[n] as usize;
        let mut edges = vec![0u32; total];
        // Staged rows.
        let mut i = 0usize;
        while i < pairs.len() {
            let src = pairs[i].0 as usize;
            let mut j = i;
            while j < pairs.len() && pairs[j].0 as usize == src {
                j += 1;
            }
            let at = offsets[src] as usize;
            for (k, p) in pairs[i..j].iter().enumerate() {
                edges[at + k] = p.1;
            }
            i = j;
        }
        // Carried rows. Indexed rather than iterated: the loop reads a row from
        // the base CSR, its destination offset from `offsets`, and writes a
        // slice of `edges` — three arrays keyed by the same index.
        #[allow(clippy::needless_range_loop)]
        for u in 0..n {
            if !self.rebuilt.contains(u) {
                let row = base.row(u as u32);
                if !row.is_empty() {
                    let at = offsets[u] as usize;
                    edges[at..at + row.len()].copy_from_slice(row);
                }
            }
        }

        Csr { offsets, edges }
    }

    /// Finalises as a delta over the base, without materialising a new index.
    ///
    /// This is the `affected` path. It costs the staged rows and the reverse
    /// corrections they imply — proportional to the change — where
    /// [`IndexBuilder::finish`] costs the whole graph. What it cannot do is
    /// produce something to publish, which is why `baseline update` still
    /// materialises.
    pub fn finish_overlay(mut self) -> Overlay<'a> {
        let mut pairs = std::mem::take(&mut self.pairs);
        sort_dedup_pairs(&mut pairs);

        // Group the staged rows by source so each rebuilt node's new row can be
        // diffed against the one it replaced.
        let mut staged: FxHashMap<u32, &[u32]> = FxHashMap::default();
        let mut flat: Vec<u32> = Vec::with_capacity(pairs.len());
        {
            let mut runs: Vec<(u32, usize, usize)> = Vec::new();
            let mut i = 0usize;
            while i < pairs.len() {
                let src = pairs[i].0;
                let start = flat.len();
                let mut j = i;
                while j < pairs.len() && pairs[j].0 == src {
                    flat.push(pairs[j].1);
                    j += 1;
                }
                runs.push((src, start, flat.len()));
                i = j;
            }
            // `flat` is complete by here and is not touched again, so the
            // slices stay valid for as long as the map does.
            for (src, a, b) in runs {
                staged.insert(src, &flat[a..b]);
            }
        }

        let mut rev_added: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        let mut rev_removed: FxHashMap<u32, FxHashSet<u32>> = FxHashMap::default();
        for u in self.rebuilt.ones().map(|i| i as u32) {
            let old = self.base_forward_row(u);
            let new: &[u32] = staged.get(&u).copied().unwrap_or(&[]);
            // Both sides are sorted and deduplicated, so the difference is a
            // linear merge rather than two set builds.
            let (mut i, mut j) = (0usize, 0usize);
            while i < old.len() || j < new.len() {
                match (old.get(i), new.get(j)) {
                    (Some(&a), Some(&b)) if a == b => {
                        i += 1;
                        j += 1;
                    }
                    (Some(&a), Some(&b)) if a < b => {
                        rev_removed.entry(a).or_default().insert(u);
                        i += 1;
                    }
                    (Some(_), Some(&b)) => {
                        rev_added.entry(b).or_default().push(u);
                        j += 1;
                    }
                    (Some(&a), None) => {
                        rev_removed.entry(a).or_default().insert(u);
                        i += 1;
                    }
                    (None, Some(&b)) => {
                        rev_added.entry(b).or_default().push(u);
                        j += 1;
                    }
                    (None, None) => unreachable!("loop guard"),
                }
            }
        }

        Overlay {
            base: self.base,
            labels: self.labels,
            nodes: self.nodes,
            rev_added,
            rev_removed,
        }
    }

    /// Finalises the index for `commit`.
    pub fn finish(mut self, commit: impl Into<String>) -> IndexData {
        let forward = self.finish_forward();
        let n = self.nodes.len() as usize;
        let reverse = if forward.edges.len() > 1 << 20 {
            Csr::reverse_of_parallel(forward.as_ref(), n)
        } else {
            Csr::reverse_of(forward.as_ref(), n)
        };

        let mut pkg_node_offsets = vec![0u32; self.pkg_nodes.len() + 1];
        for (i, r) in self.pkg_nodes.iter().enumerate() {
            pkg_node_offsets[i + 1] = r.len() as u32;
        }
        crate::csr::prefix_sum(&mut pkg_node_offsets);
        let mut pkg_nodes_flat = Vec::with_capacity(*pkg_node_offsets.last().unwrap() as usize);
        for r in &self.pkg_nodes {
            pkg_nodes_flat.extend_from_slice(r);
        }

        self.read_base_bzl();
        let sky = match self.take_sky_layer() {
            Some(l) => l.finish(),
            None => crate::sky::SkyTables::empty(),
        };
        let mut repo_boundary_bytes = Vec::new();
        let mut repo_boundary_offsets = vec![0u32];
        let mut repo_boundary_ignored = Vec::with_capacity(self.boundaries.len());
        let mut repo_boundary_traversed = Vec::with_capacity(self.boundaries.len());
        for b in &self.boundaries {
            repo_boundary_bytes.extend_from_slice(b.dir.as_bytes());
            repo_boundary_offsets.push(repo_boundary_bytes.len() as u32);
            repo_boundary_ignored.push(b.bazelignored);
            repo_boundary_traversed.push(b.traversed);
        }
        let mut round_trip_via_bytes = Vec::new();
        let mut round_trip_via_offsets = vec![0u32];
        for v in &self.round_trip_via {
            round_trip_via_bytes.extend_from_slice(v.as_bytes());
            round_trip_via_offsets.push(round_trip_via_bytes.len() as u32);
        }
        let mut leaf_closure_repo_bytes = Vec::new();
        let mut leaf_closure_repo_offsets = vec![0u32];
        let mut leaf_closure_offsets = vec![0u32];
        let mut leaf_closure_leaves = Vec::new();
        for (r, leaves) in &self.leaf_closure {
            leaf_closure_repo_bytes.extend_from_slice(r.as_bytes());
            leaf_closure_repo_offsets.push(leaf_closure_repo_bytes.len() as u32);
            leaf_closure_leaves.extend(leaves.iter().copied());
            leaf_closure_offsets.push(leaf_closure_leaves.len() as u32);
        }
        let columns = self.nodes.finish();
        let bzl = finish_bzl(self.bzl.take(), &self.pkg_names);
        let (label_bytes, label_offsets, label_sorted) = self.labels.finish();
        let (class_bytes, class_offsets) = self.classes.into_parts();
        let (pkg_name_bytes, pkg_name_offsets) = self.pkg_names.into_parts();

        let mut counts = Counts {
            nodes: n as u32,
            edges: forward.edges.len() as u64,
            packages: (pkg_name_offsets.len() - 1) as u32,
            ..Default::default()
        };
        for i in 0..n {
            match NodeKind::from_u8(columns.kind[i]) {
                NodeKind::Rule => {
                    counts.rules += 1;
                    if columns.is_test[i] {
                        counts.tests += 1;
                    }
                }
                NodeKind::SourceFile | NodeKind::GeneratedFile => counts.files += 1,
                _ => {}
            }
        }

        IndexData {
            header: Header {
                format_version: crate::format::FORMAT_VERSION,
                lineage: self.lineage,
                bazel_version: self.bazel_version,
                config_hash: self.config_hash,
                bzl_closure: bzl.present,
                sky_layer: sky.present,
                counts,
            },
            provenance: Provenance {
                commit: commit.into(),
                partial: self.partial,
                packages_in_error: self.packages_in_error,
                error_packages: self.error_packages,
                created_unix: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            },
            label_bytes,
            label_offsets,
            label_sorted,
            class_bytes,
            class_offsets,
            node_kind: columns.kind,
            node_package: columns.package,
            node_class: columns.class,
            node_hash: columns.hash,
            node_is_test: columns.is_test,
            node_flags: columns.flags,
            fwd_offsets: forward.offsets,
            fwd_edges: forward.edges,
            rev_offsets: reverse.offsets,
            rev_edges: reverse.edges,
            pkg_name_bytes,
            pkg_name_offsets,
            pkg_node_offsets,
            pkg_nodes: pkg_nodes_flat,
            pkg_bzl_hash: self.pkg_bzl_hash,
            pkg_build_hash: self.pkg_build_hash,
            pkg_flags: self.pkg_flags,
            bzl_path_bytes: bzl.bzl_path_bytes,
            bzl_path_offsets: bzl.bzl_path_offsets,
            bzl_load_offsets: bzl.bzl_load_offsets,
            bzl_load_edges: bzl.bzl_load_edges,
            bzl_consumer_offsets: bzl.bzl_consumer_offsets,
            bzl_consumer_edges: bzl.bzl_consumer_edges,
            bzl_workspace: bzl.bzl_workspace,
            bzl_ext_label_bytes: bzl.bzl_ext_label_bytes,
            bzl_ext_label_offsets: bzl.bzl_ext_label_offsets,
            bzl_ext_offsets: bzl.bzl_ext_offsets,
            bzl_ext_edges: bzl.bzl_ext_edges,
            pkg_ext_offsets: bzl.pkg_ext_offsets,
            pkg_ext_edges: bzl.pkg_ext_edges,
            repo_name_bytes: bzl.repo_name_bytes,
            repo_name_offsets: bzl.repo_name_offsets,
            repo_canonical_bytes: bzl.repo_canonical_bytes,
            repo_canonical_offsets: bzl.repo_canonical_offsets,
            repo_origin_kind_bytes: bzl.repo_origin_kind_bytes,
            repo_origin_kind_offsets: bzl.repo_origin_kind_offsets,
            repo_origin_dir_bytes: bzl.repo_origin_dir_bytes,
            repo_origin_dir_offsets: bzl.repo_origin_dir_offsets,
            repo_origin_evidence_bytes: bzl.repo_origin_evidence_bytes,
            repo_origin_evidence_offsets: bzl.repo_origin_evidence_offsets,
            repo_confirmed: bzl.repo_confirmed,
            repo_indexed: bzl.repo_indexed,
            repo_input_bytes: bzl.repo_input_bytes,
            repo_input_offsets: bzl.repo_input_offsets,
            repo_input_prefix: bzl.repo_input_prefix,
            repo_input_target_only: bzl.repo_input_target_only,
            repo_input_repo_offsets: bzl.repo_input_repo_offsets,
            repo_input_repo_edges: bzl.repo_input_repo_edges,
            bzl_repo_offsets: bzl.bzl_repo_offsets,
            bzl_repo_edges: bzl.bzl_repo_edges,
            repo_map_bytes: bzl.repo_map_bytes,
            repo_map_offsets: bzl.repo_map_offsets,
            repo_map_ranges: bzl.repo_map_ranges,
            repo_consumer_bytes: bzl.repo_consumer_bytes,
            repo_consumer_offsets: bzl.repo_consumer_offsets,
            repo_consumer_ranges: bzl.repo_consumer_ranges,
            repo_consumer_leaf_bytes: bzl.repo_consumer_leaf_bytes,
            repo_consumer_leaf_offsets: bzl.repo_consumer_leaf_offsets,
            repo_consumer_leaf_ranges: bzl.repo_consumer_leaf_ranges,
            repo_boundary_bytes,
            repo_boundary_offsets,
            repo_boundary_ignored,
            repo_boundary_traversed,
            round_trip_via_bytes,
            round_trip_via_offsets,
            leaf_closure_repo_bytes,
            leaf_closure_repo_offsets,
            leaf_closure_offsets,
            leaf_closure_leaves,
            sky_root_bytes: sky.root_bytes,
            sky_root_offsets: sky.root_offsets,
            sky_key_bytes: sky.key_bytes,
            sky_key_offsets: sky.key_offsets,
            sky_kind: sky.kind,
            sky_root: sky.root,
            sky_rdep_offsets: sky.rdep_offsets,
            sky_rdep_edges: sky.rdep_edges,
        }
    }
}

/// The serialised form of the `.bzl` load closure.
///
/// Every field is named exactly as its [`IndexData`] counterpart, so the move
/// in `finish` reads `x: bzl.x` throughout. It used to rename in flight —
/// `bzl_repo_offsets: bzl.repo_bzl_offsets` next to twenty-odd sibling lines —
/// where crossing two same-typed `Vec<u32>` fields would have compiled and
/// silently produced an index whose repository rows were its load rows.
#[derive(Default)]
struct BzlTables {
    present: bool,
    bzl_path_bytes: Vec<u8>,
    bzl_path_offsets: Vec<u32>,
    bzl_load_offsets: Vec<u32>,
    bzl_load_edges: Vec<u32>,
    bzl_consumer_offsets: Vec<u32>,
    bzl_consumer_edges: Vec<u32>,
    bzl_workspace: Vec<u32>,
    bzl_ext_label_bytes: Vec<u8>,
    bzl_ext_label_offsets: Vec<u32>,
    bzl_ext_offsets: Vec<u32>,
    bzl_ext_edges: Vec<u32>,
    pkg_ext_offsets: Vec<u32>,
    pkg_ext_edges: Vec<u32>,
    repo_name_bytes: Vec<u8>,
    repo_name_offsets: Vec<u32>,
    repo_canonical_bytes: Vec<u8>,
    repo_canonical_offsets: Vec<u32>,
    repo_origin_kind_bytes: Vec<u8>,
    repo_origin_kind_offsets: Vec<u32>,
    repo_origin_dir_bytes: Vec<u8>,
    repo_origin_dir_offsets: Vec<u32>,
    repo_origin_evidence_bytes: Vec<u8>,
    repo_origin_evidence_offsets: Vec<u32>,
    repo_confirmed: Vec<bool>,
    repo_indexed: Vec<bool>,
    repo_input_bytes: Vec<u8>,
    repo_input_offsets: Vec<u32>,
    repo_input_prefix: Vec<bool>,
    repo_input_target_only: Vec<bool>,
    repo_input_repo_offsets: Vec<u32>,
    repo_input_repo_edges: Vec<u32>,
    bzl_repo_offsets: Vec<u32>,
    bzl_repo_edges: Vec<u32>,
    repo_map_bytes: Vec<u8>,
    repo_map_offsets: Vec<u32>,
    repo_map_ranges: Vec<u32>,
    repo_consumer_bytes: Vec<u8>,
    repo_consumer_offsets: Vec<u32>,
    repo_consumer_ranges: Vec<u32>,
    repo_consumer_leaf_bytes: Vec<u8>,
    repo_consumer_leaf_offsets: Vec<u32>,
    repo_consumer_leaf_ranges: Vec<u32>,
}

impl BzlTables {
    fn empty() -> Self {
        Self {
            bzl_path_offsets: vec![0],
            bzl_load_offsets: vec![0],
            bzl_consumer_offsets: vec![0],
            bzl_ext_label_offsets: vec![0],
            bzl_ext_offsets: vec![0],
            pkg_ext_offsets: vec![0],
            repo_name_offsets: vec![0],
            repo_canonical_offsets: vec![0],
            repo_origin_kind_offsets: vec![0],
            repo_origin_dir_offsets: vec![0],
            repo_origin_evidence_offsets: vec![0],
            repo_input_offsets: vec![0],
            repo_input_repo_offsets: vec![0],
            bzl_repo_offsets: vec![0],
            repo_map_offsets: vec![0],
            repo_map_ranges: vec![0],
            repo_consumer_offsets: vec![0],
            repo_consumer_ranges: vec![0],
            repo_consumer_leaf_offsets: vec![0],
            repo_consumer_leaf_ranges: vec![0],
            ..Default::default()
        }
    }
}

/// Sorts the load graph into path order, inverts the per-package closures, and
/// flattens everything into the index's tables.
///
/// The ids the builder worked with are insertion-ordered, which depends on the
/// order packages were scanned in; sorting by path here is what makes the
/// payload a function of the repository's content rather than of the walk, and
/// what lets `IndexRef::bzl_id` binary-search.
fn finish_bzl(c: Option<BzlClosure>, pkg_names: &Interner) -> BzlTables {
    let Some(c) = c else {
        return BzlTables::empty();
    };
    let n = c.graph.len();
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_by(|&a, &b| c.graph.path(a).cmp(c.graph.path(b)));
    let mut remap = vec![0u32; n];
    for (new, &old) in order.iter().enumerate() {
        remap[old as usize] = new as u32;
    }

    let mut t = BzlTables::empty();
    t.present = true;
    let mut ext_labels = Interner::new();
    for &old in &order {
        t.bzl_path_bytes
            .extend_from_slice(c.graph.path(old).as_bytes());
        t.bzl_path_offsets.push(t.bzl_path_bytes.len() as u32);
        let mut row: Vec<u32> = c
            .graph
            .loads(old)
            .iter()
            .map(|&d| remap[d as usize])
            .collect();
        row.sort_unstable();
        row.dedup();
        t.bzl_load_edges.extend_from_slice(&row);
        t.bzl_load_offsets.push(t.bzl_load_edges.len() as u32);
        let mut ext: Vec<u32> = c
            .graph
            .external(old)
            .iter()
            .map(|l| ext_labels.intern(l))
            .collect();
        ext.sort_unstable();
        ext.dedup();
        t.bzl_ext_edges.extend_from_slice(&ext);
        t.bzl_ext_offsets.push(t.bzl_ext_edges.len() as u32);
    }
    // The BUILD files' own external loads, one row per package id, into the
    // same label table.
    let mut per_pkg: Vec<Vec<u32>> = vec![Vec::new(); pkg_names.len()];
    for (pkg, labels) in &c.package_external {
        let Some(p) = pkg_names.lookup(pkg) else {
            continue;
        };
        let row = &mut per_pkg[p as usize];
        row.extend(labels.iter().map(|l| ext_labels.intern(l)));
        row.sort_unstable();
        row.dedup();
    }
    for row in &per_pkg {
        t.pkg_ext_edges.extend_from_slice(row);
        t.pkg_ext_offsets.push(t.pkg_ext_edges.len() as u32);
    }
    let (b, o) = ext_labels.into_parts();
    t.bzl_ext_label_bytes = b;
    t.bzl_ext_label_offsets = o;

    // Invert: package closures -> consumers per `.bzl`.
    let mut consumers: Vec<Vec<u32>> = vec![Vec::new(); n];
    for (pkg, ids) in &c.packages {
        // A package the splice deleted has no slot left; its closure goes with
        // it.
        let Some(p) = pkg_names.lookup(pkg) else {
            continue;
        };
        for &id in ids {
            consumers[remap[id as usize] as usize].push(p);
        }
    }
    for row in &mut consumers {
        row.sort_unstable();
        row.dedup();
        t.bzl_consumer_edges.extend_from_slice(row);
        t.bzl_consumer_offsets
            .push(t.bzl_consumer_edges.len() as u32);
    }

    t.bzl_workspace = c.workspace.iter().map(|&i| remap[i as usize]).collect();
    t.bzl_workspace.sort_unstable();
    t.bzl_workspace.dedup();

    // ---- external repositories ----
    //
    // Repo ids are assigned in name order and pattern ids in pattern order,
    // for the same reason the `.bzl` ids are: the payload has to be a function
    // of the repository's content, not of the order anything was scanned in.
    let names: Vec<&String> = c.inputs.repos.iter().collect();
    let ids: std::collections::BTreeMap<&str, u32> = names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), i as u32))
        .collect();
    let repo_id = |n: &str| ids.get(n).copied();
    for n in &names {
        t.repo_name_bytes.extend_from_slice(n.as_bytes());
        t.repo_name_offsets.push(t.repo_name_bytes.len() as u32);
        let canon = c.inputs.canonical.get(*n).map(String::as_str).unwrap_or("");
        t.repo_canonical_bytes.extend_from_slice(canon.as_bytes());
        t.repo_canonical_offsets
            .push(t.repo_canonical_bytes.len() as u32);
        // What the repository is, in the same repo-id order. A repository no
        // claim was made about writes the empty spelling, which reads back as
        // `Declared`.
        let o = c.inputs.origins.get(*n).cloned().unwrap_or_default();
        t.repo_origin_kind_bytes
            .extend_from_slice(o.kind.as_str().as_bytes());
        t.repo_origin_kind_offsets
            .push(t.repo_origin_kind_bytes.len() as u32);
        t.repo_origin_dir_bytes.extend_from_slice(o.dir.as_bytes());
        t.repo_origin_dir_offsets
            .push(t.repo_origin_dir_bytes.len() as u32);
        t.repo_origin_evidence_bytes
            .extend_from_slice(o.evidence.as_bytes());
        t.repo_origin_evidence_offsets
            .push(t.repo_origin_evidence_bytes.len() as u32);
        t.repo_confirmed.push(o.confirmed);
        t.repo_indexed.push(o.indexed);
        // The path mapping, in the same repo-id order. An empty run is the
        // "no derivable mapping" case.
        if let Some(maps) = c.inputs.path_maps.get(*n) {
            for pm in maps {
                t.repo_map_bytes.extend_from_slice(pm.encode().as_bytes());
                t.repo_map_offsets.push(t.repo_map_bytes.len() as u32);
            }
        }
        t.repo_map_ranges.push(t.repo_map_offsets.len() as u32 - 1);
        // The external consumers of this repository, one run per repo id in
        // the same order; empty for a repository nothing consumes.
        if let Some(consumers) = c.inputs.consumers.get(*n) {
            for (consumer, used) in consumers {
                let mut entry = consumer.clone();
                for p in &used.packages {
                    entry.push('\0');
                    entry.push_str(p);
                }
                t.repo_consumer_bytes.extend_from_slice(entry.as_bytes());
                t.repo_consumer_offsets
                    .push(t.repo_consumer_bytes.len() as u32);
                // Its leaf map, when there is one: the bare name first, which
                // is what says "known" for a map that names no leaf.
                if let Some(leaves) = &used.leaves {
                    t.repo_consumer_leaf_bytes
                        .extend_from_slice(consumer.as_bytes());
                    t.repo_consumer_leaf_offsets
                        .push(t.repo_consumer_leaf_bytes.len() as u32);
                    // What it was derived through, under an empty leaf.
                    if !used.via.is_empty() {
                        let mut entry = format!("{consumer}\0");
                        for v in &used.via {
                            entry.push('\0');
                            entry.push_str(v);
                        }
                        t.repo_consumer_leaf_bytes.extend_from_slice(entry.as_bytes());
                        t.repo_consumer_leaf_offsets
                            .push(t.repo_consumer_leaf_bytes.len() as u32);
                    }
                    for (leaf, pkgs) in leaves {
                        let mut entry = format!("{consumer}\0{leaf}");
                        for p in pkgs {
                            entry.push('\0');
                            entry.push_str(p);
                        }
                        t.repo_consumer_leaf_bytes.extend_from_slice(entry.as_bytes());
                        t.repo_consumer_leaf_offsets
                            .push(t.repo_consumer_leaf_bytes.len() as u32);
                    }
                }
            }
        }
        t.repo_consumer_ranges
            .push(t.repo_consumer_offsets.len() as u32 - 1);
        t.repo_consumer_leaf_ranges
            .push(t.repo_consumer_leaf_offsets.len() as u32 - 1);
    }
    for (pat, repos) in &c.inputs.patterns {
        let mut row: Vec<u32> = repos.iter().filter_map(|r| repo_id(r)).collect();
        row.sort_unstable();
        row.dedup();
        if row.is_empty() {
            continue;
        }
        t.repo_input_bytes
            .extend_from_slice(pat.as_str().as_bytes());
        t.repo_input_offsets.push(t.repo_input_bytes.len() as u32);
        t.repo_input_prefix.push(pat.is_prefix());
        t.repo_input_target_only
            .push(c.inputs.target_only.contains(pat));
        t.repo_input_repo_edges.extend_from_slice(&row);
        t.repo_input_repo_offsets
            .push(t.repo_input_repo_edges.len() as u32);
    }
    let mut per_bzl: Vec<Vec<u32>> = vec![Vec::new(); n];
    for (path, repos) in &c.inputs.bzl_repos {
        let Some(old) = c.graph.id_of(path) else {
            continue;
        };
        let row = &mut per_bzl[remap[old as usize] as usize];
        row.extend(repos.iter().filter_map(|r| repo_id(r)));
        row.sort_unstable();
        row.dedup();
    }
    for row in &per_bzl {
        t.bzl_repo_edges.extend_from_slice(row);
        t.bzl_repo_offsets.push(t.bzl_repo_edges.len() as u32);
    }
    t
}

/// Mints a fresh lineage id from the system clock and a process-local counter.
pub fn new_lineage() -> [u8; 16] {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut h = blake3::Hasher::new();
    h.update(&nanos.to_le_bytes());
    h.update(&c.to_le_bytes());
    h.update(&std::process::id().to_le_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    out
}
