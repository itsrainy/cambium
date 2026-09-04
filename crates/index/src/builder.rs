//! Index construction and splicing.
//!
//! One code path serves both cases the spec describes: a cold full build is a
//! splice of every package into an empty index, and an incremental update is a
//! splice of the dirty packages into a loaded one. The difference is only how
//! many rows get rebuilt.

use fixedbitset::FixedBitSet;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::csr::{sort_dedup_pairs, Csr};
use crate::data::{flags, pkg_flags, Counts, Header, IndexData, IndexRef, NodeKind, NO_ID};
use crate::label;
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
    /// Hash of the package's transitive `.bzl` load closure. Reserved: v1 marks
    /// bzl-affected packages through `rbuildfiles` instead of hashing loads, so
    /// this is written as zero unless a caller supplies it.
    pub bzl_hash: [u8; 32],
    pub targets: Vec<TargetRow>,
}

/// What a splice changed in one package, for direct-set derivation (spec 6.7).
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

/// Mutable index. Built empty for a full build, or from a loaded index for a splice.
pub struct IndexBuilder {
    labels: Interner,
    classes: Interner,
    pkg_names: Interner,

    kind: Vec<u8>,
    package: Vec<u32>,
    class: Vec<u32>,
    hash: Vec<[u8; 32]>,
    is_test: Vec<bool>,
    flags: Vec<u8>,

    pkg_nodes: Vec<Vec<u32>>,
    pkg_bzl_hash: Vec<[u8; 32]>,
    pkg_build_hash: Vec<[u8; 32]>,
    pkg_flags: Vec<u8>,

    /// Forward rows carried over from the base index.
    base_forward: Csr,
    /// Rows replaced by this generation.
    rebuilt: FixedBitSet,
    /// Staged `(src, dst)` edges for rebuilt rows.
    pairs: Vec<(u32, u32)>,
    /// Generated file -> generating rule, for edge redirection.
    gen_rule: FxHashMap<u32, u32>,

    lineage: [u8; 16],
    bazel_version: String,
    config_hash: [u8; 32],
}

impl IndexBuilder {
    /// Empty index for a cold build. `lineage` should be freshly minted.
    pub fn new(lineage: [u8; 16]) -> Self {
        Self {
            labels: Interner::new(),
            classes: Interner::new(),
            pkg_names: Interner::new(),
            kind: Vec::new(),
            package: Vec::new(),
            class: Vec::new(),
            hash: Vec::new(),
            is_test: Vec::new(),
            flags: Vec::new(),
            pkg_nodes: Vec::new(),
            pkg_bzl_hash: Vec::new(),
            pkg_build_hash: Vec::new(),
            pkg_flags: Vec::new(),
            base_forward: Csr::empty(0),
            rebuilt: FixedBitSet::with_capacity(0),
            pairs: Vec::new(),
            gen_rule: FxHashMap::default(),
            lineage,
            bazel_version: String::new(),
            config_hash: [0; 32],
        }
    }

    /// Loads a base index into mutable form, preserving every id and the lineage.
    pub fn from_view(v: IndexRef<'_>) -> Self {
        let (lb, lo) = (v.labels.bytes.to_vec(), v.labels.offsets.to_vec());
        let (cb, co) = (v.classes.bytes.to_vec(), v.classes.offsets.to_vec());
        let (pb, po) = (v.pkg_names.bytes.to_vec(), v.pkg_names.offsets.to_vec());
        let n = v.num_nodes() as usize;
        let np = v.num_packages() as usize;
        let mut pkg_nodes = Vec::with_capacity(np);
        for p in 0..np as u32 {
            pkg_nodes.push(v.package_nodes(p).to_vec());
        }
        Self {
            labels: Interner::from_parts(lb, lo),
            classes: Interner::from_parts(cb, co),
            pkg_names: Interner::from_parts(pb, po),
            kind: v.kind.to_vec(),
            package: v.package.to_vec(),
            class: v.class.to_vec(),
            hash: v.hash.to_vec(),
            is_test: v.is_test.to_vec(),
            flags: v.flags.to_vec(),
            pkg_nodes,
            pkg_bzl_hash: v.pkg_bzl_hash.to_vec(),
            pkg_build_hash: v.pkg_build_hash.to_vec(),
            pkg_flags: v.pkg_flags.to_vec(),
            base_forward: Csr {
                offsets: v.forward.offsets.to_vec(),
                edges: v.forward.edges.to_vec(),
            },
            rebuilt: FixedBitSet::with_capacity(n),
            pairs: Vec::new(),
            gen_rule: FxHashMap::default(),
            lineage: v.lineage,
            bazel_version: v.bazel_version.to_string(),
            config_hash: v.config_hash,
        }
    }

    pub fn set_bazel_version(&mut self, v: impl Into<String>) {
        self.bazel_version = v.into();
    }

    pub fn set_config_hash(&mut self, h: [u8; 32]) {
        self.config_hash = h;
    }

    pub fn lineage(&self) -> [u8; 16] {
        self.lineage
    }

    pub fn num_nodes(&self) -> u32 {
        self.kind.len() as u32
    }

    pub fn label_id(&self, label: &str) -> Option<u32> {
        self.labels.lookup(label)
    }

    pub fn label(&self, id: u32) -> &str {
        self.labels.get(id)
    }

    pub fn package_id(&self, package: &str) -> Option<u32> {
        self.pkg_names.lookup(package)
    }

    /// Interns a label, allocating an `Unknown` node if it is new.
    fn node_for(&mut self, label: &str) -> u32 {
        let (id, is_new) = self.labels.intern_new(label);
        if is_new {
            self.kind.push(NodeKind::Unknown as u8);
            self.package.push(NO_ID);
            self.class.push(NO_ID);
            self.hash.push([0; 32]);
            self.is_test.push(false);
            self.flags.push(if label::is_external(label) {
                flags::EXTERNAL
            } else {
                0
            });
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
    /// resolves edges (including generated-file redirection).
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
                let i = id as usize;
                let was_known = self.kind[i] != NodeKind::Unknown as u8
                    && self.flags[i] & flags::TOMBSTONE == 0;
                let hash_changed = self.hash[i] != t.rule_hash;

                self.kind[i] = t.kind as u8;
                self.package[i] = pkg_id;
                self.class[i] = match &t.rule_class {
                    Some(c) => self.classes.intern(c),
                    None => NO_ID,
                };
                self.hash[i] = t.rule_hash;
                self.is_test[i] = t.is_test;
                let mut f = self.flags[i] & !(flags::TOMBSTONE | flags::MANUAL);
                if t.manual {
                    f |= flags::MANUAL;
                }
                self.flags[i] = f;

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
                let i = d as usize;
                self.kind[i] = NodeKind::Tombstone as u8;
                self.flags[i] |= flags::TOMBSTONE;
                self.class[i] = NO_ID;
                self.hash[i] = [0; 32];
                self.is_test[i] = false;
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

        // Phase 1b: record generated-file -> generating-rule for this batch.
        for up in updates {
            for t in &up.targets {
                if let (NodeKind::GeneratedFile, Some(gen)) = (t.kind, &t.generating_rule) {
                    let file_id = self.node_for(&t.label);
                    let rule_id = self.node_for(gen);
                    self.gen_rule.insert(file_id, rule_id);
                }
            }
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
                            let dst = self.redirect(dst);
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

    /// Generated files stand in for their generating rule so the reverse walk
    /// does not need an extra hop through the file node.
    fn redirect(&mut self, id: u32) -> u32 {
        if self.kind[id as usize] != NodeKind::GeneratedFile as u8 {
            return id;
        }
        if let Some(&r) = self.gen_rule.get(&id) {
            return r;
        }
        // Fall back to the base index: a generated file's forward row is exactly
        // its generating rule.
        match self.base_forward.as_ref().row(id).first() {
            Some(&r) => {
                self.gen_rule.insert(id, r);
                r
            }
            None => id,
        }
    }

    /// Marks a package as gone: tombstones everything it owned.
    /// Returns the tombstoned ids.
    pub fn delete_package(&mut self, package: &str) -> Vec<u32> {
        let Some(pkg_id) = self.pkg_names.lookup(package) else {
            return Vec::new();
        };
        let ids = std::mem::take(&mut self.pkg_nodes[pkg_id as usize]);
        for &d in &ids {
            let i = d as usize;
            self.kind[i] = NodeKind::Tombstone as u8;
            self.flags[i] |= flags::TOMBSTONE;
            self.class[i] = NO_ID;
            self.hash[i] = [0; 32];
            self.is_test[i] = false;
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
        let n = self.kind.len();
        let mut pairs = std::mem::take(&mut self.pairs);
        sort_dedup_pairs(&mut pairs);

        // Row lengths.
        let mut offsets = vec![0u32; n + 1];
        let base = self.base_forward.as_ref();
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

    /// Finalises the index for `commit`.
    pub fn finish(mut self, commit: impl Into<String>) -> IndexData {
        let forward = self.finish_forward();
        let n = self.kind.len();
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

        let (label_bytes, label_offsets) = self.labels.into_parts();
        let (class_bytes, class_offsets) = self.classes.into_parts();
        let (pkg_name_bytes, pkg_name_offsets) = self.pkg_names.into_parts();

        let mut counts = Counts {
            nodes: n as u32,
            edges: forward.edges.len() as u64,
            packages: (pkg_name_offsets.len() - 1) as u32,
            ..Default::default()
        };
        for i in 0..n {
            match NodeKind::from_u8(self.kind[i]) {
                NodeKind::Rule => {
                    counts.rules += 1;
                    if self.is_test[i] {
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
                commit: commit.into(),
                lineage: self.lineage,
                bazel_version: self.bazel_version,
                config_hash: self.config_hash,
                created_unix: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                counts,
            },
            label_bytes,
            label_offsets,
            class_bytes,
            class_offsets,
            node_kind: self.kind,
            node_package: self.package,
            node_class: self.class,
            node_hash: self.hash,
            node_is_test: self.is_test,
            node_flags: self.flags,
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
        }
    }
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
