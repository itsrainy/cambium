//! Serialised index payload and the zero-copy view over it.
//!
//! Layout is struct-of-arrays throughout so that a load is an mmap plus a
//! handful of slice casts. rkyv archives primitives as explicitly
//! little-endian `rend` wrappers; on a little-endian target those are
//! layout-identical to the native type, so the `le::` helpers hand out real
//! `&[u32]` slices without copying. Big-endian targets are rejected at compile
//! time rather than silently paying a conversion cost.

use rkyv::{Archive, Deserialize, Serialize};

use crate::csr::{Csr, CsrRef};
use crate::strtab::StrTabRef;

#[cfg(target_endian = "big")]
compile_error!("cambium index files are little-endian; big-endian hosts are unsupported");

/// Reinterpretation helpers for archived primitive slices.
pub mod le {
    use rkyv::rend::{u32_le, u64_le};

    const _: () = assert!(std::mem::size_of::<u32_le>() == 4);
    const _: () = assert!(std::mem::align_of::<u32_le>() == 4);
    const _: () = assert!(std::mem::size_of::<u64_le>() == 8);

    /// # Panics
    /// Never; on little-endian targets `u32_le` and `u32` have identical layout.
    #[inline]
    pub fn u32s(s: &[u32_le]) -> &[u32] {
        // SAFETY: identical size, alignment and (on LE) representation.
        unsafe { std::slice::from_raw_parts(s.as_ptr() as *const u32, s.len()) }
    }

    #[inline]
    pub fn u64s(s: &[u64_le]) -> &[u64] {
        // SAFETY: identical size, alignment and (on LE) representation.
        unsafe { std::slice::from_raw_parts(s.as_ptr() as *const u64, s.len()) }
    }
}

/// What a node id refers to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum NodeKind {
    Rule = 0,
    SourceFile = 1,
    GeneratedFile = 2,
    PackageGroup = 3,
    EnvironmentGroup = 4,
    /// A label referenced by some rule that we have never queried: external
    /// repositories, or targets in packages that failed to load.
    Unknown = 5,
    /// The label existed in an earlier generation and no longer does. Ids are
    /// never reused, so the row stays, emptied.
    Tombstone = 6,
}

impl NodeKind {
    #[inline]
    pub fn from_u8(v: u8) -> NodeKind {
        match v {
            0 => NodeKind::Rule,
            1 => NodeKind::SourceFile,
            2 => NodeKind::GeneratedFile,
            3 => NodeKind::PackageGroup,
            4 => NodeKind::EnvironmentGroup,
            6 => NodeKind::Tombstone,
            _ => NodeKind::Unknown,
        }
    }

    #[inline]
    pub fn is_file(self) -> bool {
        matches!(self, NodeKind::SourceFile | NodeKind::GeneratedFile)
    }
}

/// Node flag bits.
pub mod flags {
    /// Rule carries the `manual` tag.
    pub const MANUAL: u8 = 1 << 0;
    /// Label lives outside the main repository.
    pub const EXTERNAL: u8 = 1 << 1;
    /// Node is tombstoned (mirrors `NodeKind::Tombstone`, kept as a bit so the
    /// walk can mask without a kind comparison).
    pub const TOMBSTONE: u8 = 1 << 2;
    /// Rule is testonly-ish: reserved for future use by output filters.
    pub const RESERVED_3: u8 = 1 << 3;
}

/// Package flag bits.
pub mod pkg_flags {
    pub const TOMBSTONE: u8 = 1 << 0;
}

/// Sentinel used for "no rule class" / "no package".
pub const NO_ID: u32 = u32::MAX;

#[derive(Archive, Serialize, Deserialize, Clone, Debug, Default)]
pub struct Counts {
    pub nodes: u32,
    pub edges: u64,
    pub packages: u32,
    pub files: u32,
    pub rules: u32,
    pub tests: u32,
}

/// What the index *is*: everything that a caller could derive from the
/// repository's content at some revision.
///
/// Deliberately not what revision that was. Two commits whose graphs are
/// identical — a README edit, a comment, a revert — must produce byte-identical
/// payloads, so that the store can eventually address blobs by content hash
/// without a format change. The revision lives in [`Provenance`], which is
/// written to the file's trailer rather than into the archive.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct Header {
    pub format_version: u32,
    /// Random per-lineage id. Splices preserve it; full builds mint a new one.
    pub lineage: [u8; 16],
    pub bazel_version: String,
    /// Hash of the `cambium.toml` that produced this index.
    pub config_hash: [u8; 32],
    /// This index carries a static `.bzl` load closure (see [`IndexData`]).
    ///
    /// Recorded rather than inferred from an empty table: a repository with no
    /// `.bzl` files at all is indistinguishable from one built by a row source
    /// that could not read the tree, and the two want opposite answers — the
    /// first says "nothing loads this", the second "we do not know". Without
    /// the flag a run over the second would silently under-approximate.
    pub bzl_closure: bool,
    pub counts: Counts,
}

/// Which revision an index describes, and when it was built.
///
/// Kept out of the payload (see [`Header`]) and written to the file's trailer.
/// Skipped by rkyv rather than merely unset, so it cannot accidentally
/// contribute bytes: `crates/index/tests/index_core.rs` asserts that two
/// indexes built from the same graph at different commits have identical
/// payloads.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Provenance {
    /// Git revision this index describes, as lowercase hex.
    pub commit: String,
    pub created_unix: u64,
    /// The query that produced this index finished under `--keep_going` with
    /// packages it could not load (Bazel exit 3), so the graph is missing
    /// them. Recorded rather than fatal: a cold build and an incremental
    /// update drop exactly the same packages, so the two still agree — but a
    /// consumer comparing an index against reality deserves to know.
    pub partial: bool,
    /// How many `error loading package` / `package contains errors` lines the
    /// query emitted. Not the same as "packages missing from the index": a
    /// package can produce several lines.
    pub packages_in_error: u32,
    /// The first few package names from those lines, for diagnosis.
    pub error_packages: Vec<String>,
}

/// The serialised payload. Every field is a flat array addressable from mmap.
#[derive(Archive, Serialize, Deserialize, Clone, Debug)]
pub struct IndexData {
    pub header: Header,

    /// Which revision this describes. Excluded from the archive so the payload
    /// is a function of the graph alone.
    #[rkyv(with = rkyv::with::Skip)]
    pub provenance: Provenance,

    // ---- labels ----
    pub label_bytes: Vec<u8>,
    pub label_offsets: Vec<u32>,
    /// Node ids ordered by their label, so a label can be found by binary
    /// search without building a hash map over every label in the index.
    ///
    /// Four bytes per node, and it is what lets the `affected` path resolve the
    /// couple of thousand labels a dirty query mentions in microseconds instead
    /// of the ~280 ms an O(labels) map build costs at a few million nodes.
    pub label_sorted: Vec<u32>,

    // ---- rule class names ----
    pub class_bytes: Vec<u8>,
    pub class_offsets: Vec<u32>,

    // ---- nodes (struct of arrays, all length = counts.nodes) ----
    pub node_kind: Vec<u8>,
    pub node_package: Vec<u32>,
    pub node_class: Vec<u32>,
    pub node_hash: Vec<[u8; 32]>,
    pub node_is_test: Vec<bool>,
    pub node_flags: Vec<u8>,

    // ---- edges ----
    pub fwd_offsets: Vec<u32>,
    pub fwd_edges: Vec<u32>,
    pub rev_offsets: Vec<u32>,
    pub rev_edges: Vec<u32>,

    // ---- packages ----
    pub pkg_name_bytes: Vec<u8>,
    pub pkg_name_offsets: Vec<u32>,
    pub pkg_node_offsets: Vec<u32>,
    pub pkg_nodes: Vec<u32>,
    pub pkg_bzl_hash: Vec<[u8; 32]>,
    pub pkg_build_hash: Vec<[u8; 32]>,
    pub pkg_flags: Vec<u8>,

    // ---- the static `.bzl` load closure (format 4) ----
    //
    // Three tables over one id space, whose ids are assigned in path order so
    // a path can be found by binary search:
    //
    // - `bzl_loads` is what each `.bzl` loads directly, which is what lets an
    //   incremental run recompute a dirty package's closure without re-reading
    //   the `.bzl` files whose text did not move.
    // - `bzl_consumers` is the inverted closure: the packages whose BUILD file
    //   transitively loads this `.bzl`. That is the direction `affected` asks
    //   in ("this file changed, who cares?"), so the answer is one CSR row.
    // - `bzl_workspace` is the closure of `MODULE.bazel` / `WORKSPACE`;
    //   a `.bzl` in it defines repository rules and escalates to global.
    //
    // `bzl_ext_labels` records the canonical `@@repo//pkg:f.bzl` labels the
    // walk stopped at, so a later MODULE-diffing pass can map a module version
    // bump onto the files that load it.
    pub bzl_path_bytes: Vec<u8>,
    pub bzl_path_offsets: Vec<u32>,
    pub bzl_load_offsets: Vec<u32>,
    pub bzl_load_edges: Vec<u32>,
    pub bzl_consumer_offsets: Vec<u32>,
    pub bzl_consumer_edges: Vec<u32>,
    pub bzl_workspace: Vec<u32>,
    pub bzl_ext_label_bytes: Vec<u8>,
    pub bzl_ext_label_offsets: Vec<u32>,
    pub bzl_ext_offsets: Vec<u32>,
    pub bzl_ext_edges: Vec<u32>,

    // ---- external repositories built from main-repo files (format 5) ----
    //
    // One id space of *apparent* repository names, sorted, with the canonical
    // name (`rules_cc+`) alongside so a run can match either spelling of an
    // external label without re-running `bazel mod dump_repo_mapping`.
    //
    // - `repo_input_*` is the map from a main-repo path pattern to the
    //   repositories built from it: an edit to `.github/config.yml` dirties
    //   `@envoy_repo` and nothing else. Patterns are sorted; a trailing `/`
    //   (recorded in `repo_input_prefix`) means "this directory and below".
    // - `bzl_repo_*` is, for each `.bzl` in the workspace closure, the
    //   repositories it can define. That is what stops a change to
    //   `bazel/repo.bzl` from being global.
    //
    // See [`crate::externals`] for how the two are derived and why the
    // attribution errs wide.
    pub repo_name_bytes: Vec<u8>,
    pub repo_name_offsets: Vec<u32>,
    pub repo_canonical_bytes: Vec<u8>,
    pub repo_canonical_offsets: Vec<u32>,
    pub repo_input_bytes: Vec<u8>,
    pub repo_input_offsets: Vec<u32>,
    pub repo_input_prefix: Vec<bool>,
    pub repo_input_repo_offsets: Vec<u32>,
    pub repo_input_repo_edges: Vec<u32>,
    pub bzl_repo_offsets: Vec<u32>,
    pub bzl_repo_edges: Vec<u32>,

    // ---- how main-repo paths land inside a repository (format 6) ----
    //
    // `repo_map_entries` is a string table of `from\0to` pairs (see
    // [`crate::externals::PathMap`]); `repo_map_offsets` cuts it into one run
    // per repo id. An empty run means "no mapping could be derived", which is
    // what keeps the old whole-closure answer for that repository.
    pub repo_map_bytes: Vec<u8>,
    pub repo_map_offsets: Vec<u32>,
    pub repo_map_ranges: Vec<u32>,
}

/// Borrowed view over an index, owned or archived. Every algorithm in the
/// crate takes one of these so there is a single implementation regardless of
/// whether the bytes came from an mmap or from a builder.
#[derive(Clone, Copy)]
pub struct IndexRef<'a> {
    pub format_version: u32,
    pub commit: &'a str,
    pub lineage: [u8; 16],
    pub bazel_version: &'a str,
    pub config_hash: [u8; 32],
    pub created_unix: u64,
    /// See [`Provenance::partial`].
    pub partial: bool,
    pub packages_in_error: u32,
    pub error_packages: &'a [String],

    pub labels: StrTabRef<'a>,
    /// Node ids ordered by label; see [`IndexData::label_sorted`].
    pub label_sorted: &'a [u32],
    pub classes: StrTabRef<'a>,

    pub kind: &'a [u8],
    pub package: &'a [u32],
    pub class: &'a [u32],
    pub hash: &'a [[u8; 32]],
    pub is_test: &'a [bool],
    pub flags: &'a [u8],

    pub forward: CsrRef<'a>,
    pub reverse: CsrRef<'a>,

    pub pkg_names: StrTabRef<'a>,
    pub pkg_nodes: CsrRef<'a>,
    pub pkg_bzl_hash: &'a [[u8; 32]],
    pub pkg_build_hash: &'a [[u8; 32]],
    pub pkg_flags: &'a [u8],

    /// Whether the `.bzl` tables below mean anything; see [`Header::bzl_closure`].
    pub bzl_closure: bool,
    /// `.bzl` paths, sorted, one per id.
    pub bzl_paths: StrTabRef<'a>,
    /// What each `.bzl` loads directly.
    pub bzl_loads: CsrRef<'a>,
    /// Package ids whose closure contains each `.bzl`.
    pub bzl_consumers: CsrRef<'a>,
    /// `.bzl` ids reachable from `MODULE.bazel` / `WORKSPACE`.
    pub bzl_workspace: &'a [u32],
    pub bzl_ext_labels: StrTabRef<'a>,
    pub bzl_ext: CsrRef<'a>,

    /// Apparent external-repository names, sorted, one per repo id.
    pub repo_names: StrTabRef<'a>,
    /// Canonical name of each repo id, empty when unknown.
    pub repo_canonical: StrTabRef<'a>,
    /// Main-repo path patterns, sorted, one per pattern id.
    pub repo_input_patterns: StrTabRef<'a>,
    /// Whether each pattern is a directory prefix rather than an exact path.
    pub repo_input_prefix: &'a [bool],
    /// Pattern id -> repo ids.
    pub repo_inputs: CsrRef<'a>,
    /// `.bzl` id -> the repo ids that `.bzl` can define.
    pub bzl_repos: CsrRef<'a>,
    /// Encoded [`crate::externals::PathMap`] entries, grouped by repo id.
    pub repo_maps: StrTabRef<'a>,
    /// Repo id -> its run in [`IndexRef::repo_maps`]; length `num_repos + 1`.
    pub repo_map_ranges: &'a [u32],
}

impl<'a> IndexRef<'a> {
    #[inline]
    pub fn num_nodes(&self) -> u32 {
        self.kind.len() as u32
    }

    #[inline]
    pub fn num_packages(&self) -> u32 {
        self.pkg_names.len() as u32
    }

    #[inline]
    pub fn num_edges(&self) -> usize {
        self.forward.num_edges()
    }

    #[inline]
    pub fn label(&self, id: u32) -> &'a str {
        self.labels.get(id)
    }

    /// Finds a label's node id by binary search over [`IndexRef::label_sorted`].
    ///
    /// O(log n) and touches a handful of cache lines, where building the whole
    /// `label -> id` map is O(n) and touches everything. Use this when looking
    /// up a few labels; use `labels.build_map()` when looking up most of them.
    pub fn id_of(&self, label: &str) -> Option<u32> {
        self.label_sorted
            .binary_search_by(|&id| self.labels.get(id).cmp(label))
            .ok()
            .map(|i| self.label_sorted[i])
    }

    #[inline]
    pub fn kind_of(&self, id: u32) -> NodeKind {
        NodeKind::from_u8(self.kind[id as usize])
    }

    #[inline]
    pub fn is_rule(&self, id: u32) -> bool {
        self.kind[id as usize] == NodeKind::Rule as u8
    }

    #[inline]
    pub fn is_tombstone(&self, id: u32) -> bool {
        self.flags[id as usize] & flags::TOMBSTONE != 0
    }

    #[inline]
    pub fn is_manual(&self, id: u32) -> bool {
        self.flags[id as usize] & flags::MANUAL != 0
    }

    #[inline]
    pub fn rule_class(&self, id: u32) -> Option<&'a str> {
        let c = self.class[id as usize];
        (c != NO_ID).then(|| self.classes.get(c))
    }

    #[inline]
    pub fn package_of(&self, id: u32) -> Option<&'a str> {
        let p = self.package[id as usize];
        (p != NO_ID).then(|| self.pkg_names.get(p))
    }

    #[inline]
    pub fn package_id_of(&self, id: u32) -> Option<u32> {
        let p = self.package[id as usize];
        (p != NO_ID).then_some(p)
    }

    /// Nodes owned by a package. Not contiguous in id space after splices.
    #[inline]
    pub fn package_nodes(&self, pkg: u32) -> &'a [u32] {
        self.pkg_nodes.row(pkg)
    }

    #[inline]
    pub fn package_name(&self, pkg: u32) -> &'a str {
        self.pkg_names.get(pkg)
    }

    #[inline]
    pub fn package_tombstoned(&self, pkg: u32) -> bool {
        self.pkg_flags[pkg as usize] & pkg_flags::TOMBSTONE != 0
    }

    /// Rules consuming a source-file node.
    ///
    /// This is exactly the reverse row: only rules hold forward edges, so every
    /// in-edge of a file node comes from a rule. The spec's separate `files`
    /// CSR would be a byte-for-byte copy of these rows, so it is kept as a view
    /// (spec 5.3 explicitly allows this).
    #[inline]
    pub fn file_consumers(&self, id: u32) -> &'a [u32] {
        self.reverse.row(id)
    }

    // ---- the static `.bzl` load closure ----

    #[inline]
    pub fn num_bzl(&self) -> u32 {
        self.bzl_paths.len() as u32
    }

    #[inline]
    pub fn bzl_path(&self, id: u32) -> &'a str {
        self.bzl_paths.get(id)
    }

    /// Id of a repo-relative `.bzl` path. O(log n): the table is path-sorted.
    pub fn bzl_id(&self, path: &str) -> Option<u32> {
        let n = self.bzl_paths.len();
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.bzl_paths.get(mid as u32).cmp(path) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid as u32),
            }
        }
        None
    }

    #[inline]
    pub fn bzl_loads(&self, id: u32) -> &'a [u32] {
        self.bzl_loads.row(id)
    }

    /// Packages whose BUILD file transitively loads this `.bzl`. The whole
    /// point of the inverted layout: this is O(consumers), not O(packages).
    #[inline]
    pub fn bzl_consumers(&self, id: u32) -> &'a [u32] {
        self.bzl_consumers.row(id)
    }

    /// Canonical external labels this `.bzl` loads and the walk stopped at.
    pub fn bzl_external(&self, id: u32) -> Vec<&'a str> {
        self.bzl_ext
            .row(id)
            .iter()
            .map(|&e| self.bzl_ext_labels.get(e))
            .collect()
    }

    /// This `.bzl` is reachable from `MODULE.bazel` or `WORKSPACE`, so it can
    /// define repository rules.
    #[inline]
    pub fn bzl_in_workspace(&self, id: u32) -> bool {
        self.bzl_workspace.binary_search(&id).is_ok()
    }

    // ---- external repositories ----

    #[inline]
    pub fn num_repos(&self) -> u32 {
        self.repo_names.len() as u32
    }

    #[inline]
    pub fn repo_name(&self, id: u32) -> &'a str {
        self.repo_names.get(id)
    }

    /// The canonical name of a repo id, or its apparent name when the index
    /// was built without a repository mapping.
    #[inline]
    pub fn repo_canonical_name(&self, id: u32) -> &'a str {
        let c = self.repo_canonical.get(id);
        if c.is_empty() {
            self.repo_name(id)
        } else {
            c
        }
    }

    /// The repositories a changed main-repo path feeds.
    ///
    /// Linear in the number of stored patterns, which is tens on every subject
    /// entry — a prefix match cannot be answered by binary search, and the
    /// alternative (a trie) would not pay for itself at this size.
    pub fn repos_for_path(&self, path: &str) -> Vec<u32> {
        let mut out = Vec::new();
        for p in 0..self.repo_input_patterns.len() as u32 {
            let pat = self.repo_input_patterns.get(p);
            let hit = if self.repo_input_prefix[p as usize] {
                path.starts_with(pat)
            } else {
                path == pat
            };
            if hit {
                out.extend_from_slice(self.repo_inputs.row(p));
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// How main-repo paths land inside a repository, when that could be
    /// derived statically. An empty answer means "unknown", and the engine
    /// keeps its wide behaviour for that repository.
    pub fn repo_path_maps(&self, repo: u32) -> Vec<crate::externals::PathMap> {
        let i = repo as usize;
        if i + 1 >= self.repo_map_ranges.len() {
            return Vec::new();
        }
        (self.repo_map_ranges[i]..self.repo_map_ranges[i + 1])
            .filter_map(|e| crate::externals::PathMap::decode(self.repo_maps.get(e)))
            .collect()
    }

    /// The repositories a workspace-closure `.bzl` can define.
    #[inline]
    pub fn bzl_repos_of(&self, id: u32) -> &'a [u32] {
        if (id as usize) < self.bzl_repos.offsets.len().saturating_sub(1) {
            self.bzl_repos.row(id)
        } else {
            &[]
        }
    }

    pub fn counts(&self) -> Counts {
        let mut c = Counts {
            nodes: self.num_nodes(),
            edges: self.num_edges() as u64,
            packages: self.num_packages(),
            ..Default::default()
        };
        for id in 0..self.num_nodes() {
            match self.kind_of(id) {
                NodeKind::Rule => {
                    c.rules += 1;
                    if self.is_test[id as usize] {
                        c.tests += 1;
                    }
                }
                NodeKind::SourceFile | NodeKind::GeneratedFile => c.files += 1,
                _ => {}
            }
        }
        c
    }
}

impl IndexData {
    pub fn view(&self) -> IndexRef<'_> {
        IndexRef {
            format_version: self.header.format_version,
            commit: &self.provenance.commit,
            lineage: self.header.lineage,
            bazel_version: &self.header.bazel_version,
            config_hash: self.header.config_hash,
            created_unix: self.provenance.created_unix,
            partial: self.provenance.partial,
            packages_in_error: self.provenance.packages_in_error,
            error_packages: &self.provenance.error_packages,
            labels: StrTabRef {
                bytes: &self.label_bytes,
                offsets: &self.label_offsets,
            },
            label_sorted: &self.label_sorted,
            classes: StrTabRef {
                bytes: &self.class_bytes,
                offsets: &self.class_offsets,
            },
            kind: &self.node_kind,
            package: &self.node_package,
            class: &self.node_class,
            hash: &self.node_hash,
            is_test: &self.node_is_test,
            flags: &self.node_flags,
            forward: CsrRef {
                offsets: &self.fwd_offsets,
                edges: &self.fwd_edges,
            },
            reverse: CsrRef {
                offsets: &self.rev_offsets,
                edges: &self.rev_edges,
            },
            pkg_names: StrTabRef {
                bytes: &self.pkg_name_bytes,
                offsets: &self.pkg_name_offsets,
            },
            pkg_nodes: CsrRef {
                offsets: &self.pkg_node_offsets,
                edges: &self.pkg_nodes,
            },
            pkg_bzl_hash: &self.pkg_bzl_hash,
            pkg_build_hash: &self.pkg_build_hash,
            pkg_flags: &self.pkg_flags,
            bzl_closure: self.header.bzl_closure,
            bzl_paths: StrTabRef {
                bytes: &self.bzl_path_bytes,
                offsets: &self.bzl_path_offsets,
            },
            bzl_loads: CsrRef {
                offsets: &self.bzl_load_offsets,
                edges: &self.bzl_load_edges,
            },
            bzl_consumers: CsrRef {
                offsets: &self.bzl_consumer_offsets,
                edges: &self.bzl_consumer_edges,
            },
            bzl_workspace: &self.bzl_workspace,
            bzl_ext_labels: StrTabRef {
                bytes: &self.bzl_ext_label_bytes,
                offsets: &self.bzl_ext_label_offsets,
            },
            bzl_ext: CsrRef {
                offsets: &self.bzl_ext_offsets,
                edges: &self.bzl_ext_edges,
            },
            repo_names: StrTabRef {
                bytes: &self.repo_name_bytes,
                offsets: &self.repo_name_offsets,
            },
            repo_canonical: StrTabRef {
                bytes: &self.repo_canonical_bytes,
                offsets: &self.repo_canonical_offsets,
            },
            repo_input_patterns: StrTabRef {
                bytes: &self.repo_input_bytes,
                offsets: &self.repo_input_offsets,
            },
            repo_input_prefix: &self.repo_input_prefix,
            repo_inputs: CsrRef {
                offsets: &self.repo_input_repo_offsets,
                edges: &self.repo_input_repo_edges,
            },
            bzl_repos: CsrRef {
                offsets: &self.bzl_repo_offsets,
                edges: &self.bzl_repo_edges,
            },
            repo_maps: StrTabRef {
                bytes: &self.repo_map_bytes,
                offsets: &self.repo_map_offsets,
            },
            repo_map_ranges: &self.repo_map_ranges,
        }
    }

    /// Forward CSR as a borrowed pair, for splice input.
    pub fn forward(&self) -> Csr {
        Csr {
            offsets: self.fwd_offsets.clone(),
            edges: self.fwd_edges.clone(),
        }
    }
}

impl ArchivedIndexData {
    /// A view over the archive. Provenance is not in the archive — it comes
    /// from the file's trailer — so the caller supplies it.
    pub fn view_with<'a>(&'a self, provenance: &'a Provenance) -> IndexRef<'a> {
        IndexRef {
            format_version: self.header.format_version.into(),
            commit: &provenance.commit,
            lineage: self.header.lineage,
            bazel_version: &self.header.bazel_version,
            config_hash: self.header.config_hash,
            created_unix: provenance.created_unix,
            partial: provenance.partial,
            packages_in_error: provenance.packages_in_error,
            error_packages: &provenance.error_packages,
            labels: StrTabRef {
                bytes: &self.label_bytes,
                offsets: le::u32s(&self.label_offsets),
            },
            label_sorted: le::u32s(&self.label_sorted),
            classes: StrTabRef {
                bytes: &self.class_bytes,
                offsets: le::u32s(&self.class_offsets),
            },
            kind: &self.node_kind,
            package: le::u32s(&self.node_package),
            class: le::u32s(&self.node_class),
            hash: &self.node_hash,
            is_test: &self.node_is_test,
            flags: &self.node_flags,
            forward: CsrRef {
                offsets: le::u32s(&self.fwd_offsets),
                edges: le::u32s(&self.fwd_edges),
            },
            reverse: CsrRef {
                offsets: le::u32s(&self.rev_offsets),
                edges: le::u32s(&self.rev_edges),
            },
            pkg_names: StrTabRef {
                bytes: &self.pkg_name_bytes,
                offsets: le::u32s(&self.pkg_name_offsets),
            },
            pkg_nodes: CsrRef {
                offsets: le::u32s(&self.pkg_node_offsets),
                edges: le::u32s(&self.pkg_nodes),
            },
            pkg_bzl_hash: &self.pkg_bzl_hash,
            pkg_build_hash: &self.pkg_build_hash,
            pkg_flags: &self.pkg_flags,
            bzl_closure: self.header.bzl_closure,
            bzl_paths: StrTabRef {
                bytes: &self.bzl_path_bytes,
                offsets: le::u32s(&self.bzl_path_offsets),
            },
            bzl_loads: CsrRef {
                offsets: le::u32s(&self.bzl_load_offsets),
                edges: le::u32s(&self.bzl_load_edges),
            },
            bzl_consumers: CsrRef {
                offsets: le::u32s(&self.bzl_consumer_offsets),
                edges: le::u32s(&self.bzl_consumer_edges),
            },
            bzl_workspace: le::u32s(&self.bzl_workspace),
            bzl_ext_labels: StrTabRef {
                bytes: &self.bzl_ext_label_bytes,
                offsets: le::u32s(&self.bzl_ext_label_offsets),
            },
            bzl_ext: CsrRef {
                offsets: le::u32s(&self.bzl_ext_offsets),
                edges: le::u32s(&self.bzl_ext_edges),
            },
            repo_names: StrTabRef {
                bytes: &self.repo_name_bytes,
                offsets: le::u32s(&self.repo_name_offsets),
            },
            repo_canonical: StrTabRef {
                bytes: &self.repo_canonical_bytes,
                offsets: le::u32s(&self.repo_canonical_offsets),
            },
            repo_input_patterns: StrTabRef {
                bytes: &self.repo_input_bytes,
                offsets: le::u32s(&self.repo_input_offsets),
            },
            repo_input_prefix: &self.repo_input_prefix,
            repo_inputs: CsrRef {
                offsets: le::u32s(&self.repo_input_repo_offsets),
                edges: le::u32s(&self.repo_input_repo_edges),
            },
            bzl_repos: CsrRef {
                offsets: le::u32s(&self.bzl_repo_offsets),
                edges: le::u32s(&self.bzl_repo_edges),
            },
            repo_maps: StrTabRef {
                bytes: &self.repo_map_bytes,
                offsets: le::u32s(&self.repo_map_offsets),
            },
            repo_map_ranges: le::u32s(&self.repo_map_ranges),
        }
    }
}
