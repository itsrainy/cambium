//! Serialised index payload and the zero-copy view over it.
//!
//! Layout is struct-of-arrays throughout so that a load is an mmap plus a
//! handful of slice casts. rkyv archives primitives as explicitly
//! little-endian `rend` wrappers; on a little-endian target those are
//! layout-identical to the native type, so the `le::` helpers hand out real
//! `&[u32]` slices without copying. Big-endian targets are rejected at compile
//! time rather than silently paying a conversion cost.

use rkyv::{Archive, Deserialize, Serialize};

use crate::csr::CsrRef;
use crate::space::NodeMeta;
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
///
/// Bit 1 was `EXTERNAL` ("label lives outside the main repository"). No
/// reader ever consulted it — `label::is_external` answers from the label —
/// so writers stopped setting it; files written before that may still carry
/// the bit, and it is ignored. Bit 3 was reserved and never written. Neither
/// retirement moves the layout, so neither bumped `FORMAT_VERSION`.
pub mod flags {
    /// Rule carries the `manual` tag.
    pub const MANUAL: u8 = 1 << 0;
    /// Node is tombstoned (mirrors `NodeKind::Tombstone`, kept as a bit so the
    /// walk can mask without a kind comparison).
    pub const TOMBSTONE: u8 = 1 << 2;
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
    /// This index carries a Skyframe loading layer (see [`crate::sky`]).
    ///
    /// Recorded for the same reason `bzl_closure` is: a repository whose
    /// loading phase touched nothing and an index built without a dump are the
    /// same empty arrays, and only one of them can be answered from. A run
    /// whose base has no layer fails with a reason rather than falling back to
    /// a model of the rules the layer replaced.
    pub sky_layer: bool,
    pub counts: Counts,
}

/// Which revision an index describes, and when it was built.
///
/// Kept out of the payload (see [`Header`]) and written to the file's trailer.
/// Skipped by rkyv rather than merely unset, so it cannot accidentally
/// contribute bytes: `crates/engine/tests/store_layout.rs` asserts that two
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
    // Format numbers in the comments below name the version that added a
    // section; `crate::format::FORMAT_VERSION` carries the authoritative
    // history.
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
    // bump onto the files that load it. `bzl_ext` is that per `.bzl`;
    // `pkg_ext` (format 7) is the same per *package*, for the labels a BUILD
    // file loads directly, which have no `.bzl` row to live on.
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
    pub pkg_ext_offsets: Vec<u32>,
    pub pkg_ext_edges: Vec<u32>,

    // ---- external repositories built from main-repo files (format 5) ----
    //
    // One id space of *apparent* repository names, sorted, with the canonical
    // name (`rules_cc+`) alongside so a run can match either spelling of an
    // external label without re-running `bazel mod dump_repo_mapping`.
    //
    // - `repo_input_*` is the map from a main-repo path pattern to the
    //   repositories built from it: an edit to `.github/config.yml` dirties
    //   `@envoy_repo` and nothing else. Patterns are sorted; a trailing `/`
    //   (recorded in `repo_input_prefix`) means "this directory and below",
    //   and a *leading* `//` means the entry is a main-repo label rather than
    //   a path — a target the repository names, whose own inputs only the
    //   graph knows (see [`crate::externals::Pattern`]). No repo-relative
    //   path begins with `//`, which is what lets the two share the column.
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
    // ---- what each repository is (format 12) ----
    //
    // Five columns in repo-id order, the identification half of issue #22.
    // `repo_origin_kind` is the stored spelling of
    // [`crate::externals::OriginKind`] (empty means nothing was derived);
    // `repo_origin_dir` is the tree directory a `Dir` repository is, with its
    // trailing `/`, or the repo-relative path of an `Overlay`'s BUILD file;
    // `repo_origin_evidence` is where the claim was read, for
    // `cambi inspect externals` and for nothing else.
    //
    // `repo_confirmed` says Bazel was asked to honour the claim and did, and
    // `repo_indexed` says the repository's own packages are in this index.
    // Both are written `false` by every build today: the probe that sets the
    // first and the universe change that sets the second are later steps of
    // the same phase, and the columns exist now so that neither needs a
    // format bump of its own.
    pub repo_origin_kind_bytes: Vec<u8>,
    pub repo_origin_kind_offsets: Vec<u32>,
    pub repo_origin_dir_bytes: Vec<u8>,
    pub repo_origin_dir_offsets: Vec<u32>,
    pub repo_origin_evidence_bytes: Vec<u8>,
    pub repo_origin_evidence_offsets: Vec<u32>,
    pub repo_confirmed: Vec<bool>,
    pub repo_indexed: Vec<bool>,
    pub repo_input_bytes: Vec<u8>,
    pub repo_input_offsets: Vec<u32>,
    pub repo_input_prefix: Vec<bool>,
    // - `repo_input_target_only` (format 8) marks, per pattern, one whose
    //   every derivation was a label embedded in text the repository rule
    //   writes — a generated BUILD file's `srcs` — rather than a label the
    //   rule resolves. Such a path moves the repository's targets, never a
    //   `.bzl` in it, so the engine applies rule (1) alone for it. See
    //   [`crate::externals::RepoInputs::target_only`].
    pub repo_input_target_only: Vec<bool>,
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

    // ---- external consumers of an indexed repository (format 13) ----
    //
    // `repo_consumer_entries` is a string table of `consumer\0pkg\0pkg...`
    // runs (the consumer repository's apparent name, then the packages of
    // the indexed repository its rules depend on); `repo_consumer_ranges`
    // cuts it into one run per repo id, the id being the *indexed*
    // repository's. See `RepoInputs::consumers`.
    pub repo_consumer_bytes: Vec<u8>,
    pub repo_consumer_offsets: Vec<u32>,
    pub repo_consumer_ranges: Vec<u32>,

    // ---- repository boundaries in the main repository's tree (format 9) ----
    //
    // Directories below the root that carry a `WORKSPACE`, `WORKSPACE.bazel`,
    // `MODULE.bazel` or `REPO.bazel` of their own. Bazel's `//...` usually
    // stops at them, so the index has no package under one, and the classifier
    // needs the list to keep from minting package names there out of changed
    // paths. Usually, not always: Bazel 8.7 walks straight past xla's
    // `third_party/tsl/REPO.bazel` and loads main-repo packages under it, so
    // which of these directories `//...` really stopped at is read back from
    // `repo_boundary_traversed` rather than from the marker.
    // Sorted, repo-relative, no trailing slash. An empty table means the
    // build never looked, which is how every index written before this reads.
    pub repo_boundary_bytes: Vec<u8>,
    pub repo_boundary_offsets: Vec<u32>,
    // `repo_boundary_ignored` (format 10) marks, per boundary, one that
    // `.bazelignore` names: Bazel does not read the directory at all, so a
    // change there that no repository claims moves nothing, where the same
    // change under a boundary Bazel does read is a hole in the graph.
    pub repo_boundary_ignored: Vec<bool>,
    // `repo_boundary_traversed` (format 12) marks, per boundary, one the cold
    // universe query returned a main-repo package under. Which markers stop
    // `//...` differs by Bazel version, so the flag is what this Bazel did
    // rather than what the marker table predicts.
    pub repo_boundary_traversed: Vec<bool>,

    // ---- the Skyframe loading layer (format 11) ----
    //
    // Bazel's own loading-phase graph, taken from `bazel dump
    // --skyframe=deps` and kept as reverse edges. One node table, ordered by
    // `(kind, root, key)` so a seed is a binary search, and one CSR of the
    // nodes that depend on each node. See [`crate::sky`] for the kinds, the
    // spellings and why the singleton kinds are dropped.
    //
    // `sky_root_*` is the root table: root 0 is the workspace and is stored as
    // the empty string, so no absolute path of the machine that built the
    // index is in the payload. `sky_key_*` holds the relative path of a rooted
    // key and the normalised key text of an unrooted one.
    pub sky_root_bytes: Vec<u8>,
    pub sky_root_offsets: Vec<u32>,
    pub sky_key_bytes: Vec<u8>,
    pub sky_key_offsets: Vec<u32>,
    pub sky_kind: Vec<u8>,
    pub sky_root: Vec<u32>,
    pub sky_rdep_offsets: Vec<u32>,
    pub sky_rdep_edges: Vec<u32>,
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
    /// External labels each *package's* BUILD file loads directly, into
    /// `bzl_ext_labels`. One row per package id.
    pub pkg_ext: CsrRef<'a>,

    /// Apparent external-repository names, sorted, one per repo id.
    pub repo_names: StrTabRef<'a>,
    /// Canonical name of each repo id, empty when unknown.
    pub repo_canonical: StrTabRef<'a>,
    /// What each repository is: the stored spelling of
    /// [`crate::externals::OriginKind`], one per repo id.
    pub repo_origin_kind: StrTabRef<'a>,
    /// The directory or BUILD-file path of that origin, empty when it has
    /// none.
    pub repo_origin_dir: StrTabRef<'a>,
    /// Where the origin was read; human-facing only.
    pub repo_origin_evidence: StrTabRef<'a>,
    /// Whether Bazel has honoured the origin claim.
    pub repo_confirmed: &'a [bool],
    /// Whether the repository's own packages are in this index.
    pub repo_indexed: &'a [bool],
    /// Main-repo path patterns, sorted, one per pattern id.
    pub repo_input_patterns: StrTabRef<'a>,
    /// Whether each pattern is a directory prefix rather than an exact path.
    pub repo_input_prefix: &'a [bool],
    pub repo_input_target_only: &'a [bool],
    /// Pattern id -> repo ids.
    pub repo_inputs: CsrRef<'a>,
    /// `.bzl` id -> the repo ids that `.bzl` can define.
    pub bzl_repos: CsrRef<'a>,
    /// Encoded [`crate::externals::PathMap`] entries, grouped by repo id.
    pub repo_maps: StrTabRef<'a>,
    /// Repo id -> its run in [`IndexRef::repo_maps`]; length `num_repos + 1`.
    pub repo_map_ranges: &'a [u32],
    /// `consumer\0pkg\0pkg...` runs, grouped by the indexed repo's id.
    pub repo_consumers: StrTabRef<'a>,
    /// Repo id -> its run in [`IndexRef::repo_consumers`]; length
    /// `num_repos + 1`.
    pub repo_consumer_ranges: &'a [u32],
    /// Directories of the main repository's tree that are Bazel repositories
    /// of their own, sorted, without a trailing slash.
    pub repo_boundaries: StrTabRef<'a>,
    /// Per boundary, whether `.bazelignore` names its directory.
    pub repo_boundary_ignored: &'a [bool],
    /// Per boundary, whether `//...` loaded a main-repo package under it.
    pub repo_boundary_traversed: &'a [bool],

    /// Bazel's own loading-phase graph; see [`crate::sky`].
    pub sky: crate::sky::SkyRef<'a>,
}

impl<'a> IndexRef<'a> {
    /// Checks every string table's invariants, so that the rest of the crate
    /// can read entries without re-validating UTF-8 on each lookup.
    ///
    /// Called once per file at open; see [`StrTabRef::validate`].
    pub fn validate_strings(&self) -> std::result::Result<(), String> {
        for (what, t) in [
            ("labels", self.labels),
            ("classes", self.classes),
            ("pkg_names", self.pkg_names),
            ("bzl_paths", self.bzl_paths),
            ("bzl_ext_labels", self.bzl_ext_labels),
            ("repo_names", self.repo_names),
            ("repo_canonical", self.repo_canonical),
            ("repo_origin_kind", self.repo_origin_kind),
            ("repo_origin_dir", self.repo_origin_dir),
            ("repo_origin_evidence", self.repo_origin_evidence),
            ("repo_input_patterns", self.repo_input_patterns),
            ("repo_maps", self.repo_maps),
            ("repo_consumers", self.repo_consumers),
            ("repo_boundaries", self.repo_boundaries),
            ("sky_roots", self.sky.roots),
            ("sky_keys", self.sky.keys),
        ] {
            t.validate(what)?;
        }
        Ok(())
    }

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

    /// Every node column for `id`, gathered. The one place the struct-of-arrays
    /// layout is folded back into a record, so the builder and the overlay
    /// read a base node the same way.
    #[inline]
    pub fn meta(&self, id: u32) -> NodeMeta {
        let i = id as usize;
        NodeMeta {
            kind: self.kind[i],
            package: self.package[i],
            class: self.class[i],
            hash: self.hash[i],
            is_test: self.is_test[i],
            flags: self.flags[i],
        }
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
    /// This is exactly the reverse row: a file node's in-edges all come from
    /// rules (a generated file's one forward edge goes *to* its generator), so
    /// a separate `files` CSR would be a byte-for-byte copy of these rows.
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

    /// Canonical external labels this package's BUILD file loads directly.
    /// Empty for an index without a closure, or one older than format 7.
    pub fn package_external(&self, pkg: u32) -> Vec<&'a str> {
        if pkg as usize >= self.pkg_ext.len() {
            return Vec::new();
        }
        self.pkg_ext
            .row(pkg)
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

    /// The apparent names of the repositories whose own packages this index
    /// holds, in id order.
    ///
    /// The universe query that built the index was `//...:*` plus one
    /// `@name//...:*` per name here (`cambium_bazel::universe_expression`), so
    /// this is what a refresh has to ask for to leave the layer as a cold
    /// build at the same commit would. Empty on every index built before the
    /// universe was widened, which is the behaviour those indexes were built
    /// with.
    pub fn indexed_repos(&self) -> Vec<String> {
        (0..self.num_repos())
            .filter(|&r| self.repo_origin(r).indexed)
            .map(|r| self.repo_name(r).to_string())
            .collect()
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
        self.repos_for_path_kinds(path)
            .into_iter()
            .map(|(repo, _)| repo)
            .collect()
    }

    /// As [`IndexRef::repos_for_path`], with whether the path *feeds* each
    /// repository's rule (`true`) or is only named by a target the rule writes
    /// (`false`; see [`crate::externals::RepoInputs::target_only`]). A
    /// repository named both ways feeds. Ascending by repo id, no duplicates.
    pub fn repos_for_path_kinds(&self, path: &str) -> Vec<(u32, bool)> {
        let mut out: std::collections::BTreeMap<u32, bool> = Default::default();
        for p in 0..self.repo_input_patterns.len() as u32 {
            let pat = self.repo_input_patterns.get(p);
            // A label pattern is not a path and is answered from the graph
            // instead; see [`IndexRef::repos_for_label_kinds`].
            if crate::externals::Pattern::is_stored_label(pat) {
                continue;
            }
            let hit = if self.repo_input_prefix[p as usize] {
                path.starts_with(pat)
            } else {
                path == pat
            };
            if !hit {
                continue;
            }
            let feeds = !self.repo_input_target_only[p as usize];
            for &r in self.repo_inputs.row(p) {
                *out.entry(r).or_default() |= feeds;
            }
        }
        out.into_iter().collect()
    }

    /// The repositories a main-repo *target* is an input of, in the same shape
    /// as [`IndexRef::repos_for_path_kinds`].
    ///
    /// The counterpart to the path lookup for [`crate::externals::Pattern::Label`]:
    /// the repository's generated targets name this label, so a change that
    /// reaches this target in the graph moves them. `label` is canonical
    /// `//pkg:name`.
    pub fn repos_for_label_kinds(&self, label: &str) -> Vec<(u32, bool)> {
        let mut out: std::collections::BTreeMap<u32, bool> = Default::default();
        for p in 0..self.repo_input_patterns.len() as u32 {
            if self.repo_input_patterns.get(p) != label {
                continue;
            }
            let feeds = !self.repo_input_target_only[p as usize];
            for &r in self.repo_inputs.row(p) {
                *out.entry(r).or_default() |= feeds;
            }
        }
        out.into_iter().collect()
    }

    /// Every stored [`crate::externals::Pattern::Label`], with the
    /// repositories it is an input of and whether it feeds their rules.
    ///
    /// The engine turns this into a node-id map once per run, and only when it
    /// is non-empty, so that the reverse walk can ask "did this change reach a
    /// target one of the repositories names?".
    pub fn label_inputs(&self) -> Vec<(&'a str, Vec<(u32, bool)>)> {
        let mut out = Vec::new();
        for p in 0..self.repo_input_patterns.len() as u32 {
            let pat = self.repo_input_patterns.get(p);
            if !crate::externals::Pattern::is_stored_label(pat) {
                continue;
            }
            let feeds = !self.repo_input_target_only[p as usize];
            let repos: Vec<(u32, bool)> = self
                .repo_inputs
                .row(p)
                .iter()
                .map(|&r| (r, feeds))
                .collect();
            if !repos.is_empty() {
                out.push((pat, repos));
            }
        }
        out
    }

    /// What a repository *is*, as far as this tree is concerned. A repo id
    /// past the end of the columns, which is every id of an index written
    /// before format 12, reads back as
    /// [`crate::externals::OriginKind::Declared`] with nothing beside it:
    /// the widest answer.
    pub fn repo_origin(&self, repo: u32) -> crate::externals::RepoOrigin {
        let i = repo as usize;
        if i >= self.repo_origin_kind.len() {
            return crate::externals::RepoOrigin::default();
        }
        crate::externals::RepoOrigin {
            kind: crate::externals::OriginKind::from_stored(self.repo_origin_kind.get(repo)),
            dir: self.repo_origin_dir.get(repo).to_string(),
            evidence: self.repo_origin_evidence.get(repo).to_string(),
            confirmed: self.repo_confirmed.get(i).copied().unwrap_or(false),
            indexed: self.repo_indexed.get(i).copied().unwrap_or(false),
        }
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

    /// The external repositories whose rules depend on this (indexed)
    /// repository's targets, each with the packages of it they depend on.
    /// Empty for a repository nothing outside `//...` consumes, and for
    /// every index written before format 13.
    pub fn repo_consumers(&self, repo: u32) -> Vec<(String, std::collections::BTreeSet<String>)> {
        let i = repo as usize;
        if i + 1 >= self.repo_consumer_ranges.len() {
            return Vec::new();
        }
        (self.repo_consumer_ranges[i]..self.repo_consumer_ranges[i + 1])
            .filter_map(|e| {
                let mut parts = self.repo_consumers.get(e).split('\0');
                let consumer = parts.next()?.to_string();
                Some((consumer, parts.map(str::to_string).collect()))
            })
            .collect()
    }

    /// The directories of this tree that are Bazel repositories of their own,
    /// and which `//...` therefore never loaded a package under. Sorted, and
    /// without a trailing slash. Empty for an index built before the walk
    /// existed, or by a build with no checkout to walk.
    pub fn repo_boundaries(&self) -> impl Iterator<Item = &'a str> + '_ {
        (0..self.repo_boundaries.len() as u32).map(|i| self.repo_boundaries.get(i))
    }

    /// The same directories, each with whether `.bazelignore` names it.
    pub fn boundaries(&self) -> impl Iterator<Item = crate::Boundary> + '_ {
        let flags = self.repo_boundary_ignored;
        let walked = self.repo_boundary_traversed;
        (0..self.repo_boundaries.len()).map(move |i| crate::Boundary {
            dir: self.repo_boundaries.get(i as u32).to_string(),
            bazelignored: flags.get(i).copied().unwrap_or(false),
            traversed: walked.get(i).copied().unwrap_or(false),
        })
    }

    /// Whether the boundary at `dir` is one `.bazelignore` names. False for a
    /// directory that is no boundary at all, which has no answer to give.
    pub fn boundary_is_bazelignored(&self, dir: &str) -> bool {
        (0..self.repo_boundaries.len())
            .find(|i| self.repo_boundaries.get(*i as u32) == dir)
            .is_some_and(|i| self.repo_boundary_ignored.get(i).copied().unwrap_or(false))
    }

    /// Whether `//...` loaded a main-repo package under the boundary at
    /// `dir`, so the main repository owns paths there after all. False for a
    /// directory that is no boundary at all, and for every index written
    /// before the universe answer was recorded.
    pub fn boundary_is_traversed(&self, dir: &str) -> bool {
        (0..self.repo_boundaries.len())
            .find(|i| self.repo_boundaries.get(*i as u32) == dir)
            .is_some_and(|i| self.repo_boundary_traversed.get(i).copied().unwrap_or(false))
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

/// The one field list behind [`IndexData::view`] and
/// [`ArchivedIndexData::view_with`].
///
/// The two only differ in how a stored `u32` array is reached: the owned
/// struct holds `Vec<u32>`, the archive holds `ArchivedVec<u32_le>` and needs
/// [`le::u32s`] to reinterpret it. Passing that adapter in as `$u32s` lets one
/// list serve both, so a new section is added in exactly one place instead of
/// being hand-mirrored twice.
///
/// `$d` is the data struct, `$p` the provenance, `$ver` the format version
/// (already widened out of its archived representation).
macro_rules! index_view {
    ($d:expr, $p:expr, $u32s:path, $ver:expr) => {
        IndexRef {
            format_version: $ver,
            commit: &$p.commit,
            lineage: $d.header.lineage,
            bazel_version: &$d.header.bazel_version,
            config_hash: $d.header.config_hash,
            created_unix: $p.created_unix,
            partial: $p.partial,
            packages_in_error: $p.packages_in_error,
            error_packages: &$p.error_packages,
            labels: StrTabRef {
                bytes: &$d.label_bytes,
                offsets: $u32s(&$d.label_offsets),
            },
            label_sorted: $u32s(&$d.label_sorted),
            classes: StrTabRef {
                bytes: &$d.class_bytes,
                offsets: $u32s(&$d.class_offsets),
            },
            kind: &$d.node_kind,
            package: $u32s(&$d.node_package),
            class: $u32s(&$d.node_class),
            hash: &$d.node_hash,
            is_test: &$d.node_is_test,
            flags: &$d.node_flags,
            forward: CsrRef {
                offsets: $u32s(&$d.fwd_offsets),
                edges: $u32s(&$d.fwd_edges),
            },
            reverse: CsrRef {
                offsets: $u32s(&$d.rev_offsets),
                edges: $u32s(&$d.rev_edges),
            },
            pkg_names: StrTabRef {
                bytes: &$d.pkg_name_bytes,
                offsets: $u32s(&$d.pkg_name_offsets),
            },
            pkg_nodes: CsrRef {
                offsets: $u32s(&$d.pkg_node_offsets),
                edges: $u32s(&$d.pkg_nodes),
            },
            pkg_bzl_hash: &$d.pkg_bzl_hash,
            pkg_build_hash: &$d.pkg_build_hash,
            pkg_flags: &$d.pkg_flags,
            bzl_closure: $d.header.bzl_closure,
            bzl_paths: StrTabRef {
                bytes: &$d.bzl_path_bytes,
                offsets: $u32s(&$d.bzl_path_offsets),
            },
            bzl_loads: CsrRef {
                offsets: $u32s(&$d.bzl_load_offsets),
                edges: $u32s(&$d.bzl_load_edges),
            },
            bzl_consumers: CsrRef {
                offsets: $u32s(&$d.bzl_consumer_offsets),
                edges: $u32s(&$d.bzl_consumer_edges),
            },
            bzl_workspace: $u32s(&$d.bzl_workspace),
            bzl_ext_labels: StrTabRef {
                bytes: &$d.bzl_ext_label_bytes,
                offsets: $u32s(&$d.bzl_ext_label_offsets),
            },
            bzl_ext: CsrRef {
                offsets: $u32s(&$d.bzl_ext_offsets),
                edges: $u32s(&$d.bzl_ext_edges),
            },
            pkg_ext: CsrRef {
                offsets: $u32s(&$d.pkg_ext_offsets),
                edges: $u32s(&$d.pkg_ext_edges),
            },
            repo_names: StrTabRef {
                bytes: &$d.repo_name_bytes,
                offsets: $u32s(&$d.repo_name_offsets),
            },
            repo_canonical: StrTabRef {
                bytes: &$d.repo_canonical_bytes,
                offsets: $u32s(&$d.repo_canonical_offsets),
            },
            repo_origin_kind: StrTabRef {
                bytes: &$d.repo_origin_kind_bytes,
                offsets: $u32s(&$d.repo_origin_kind_offsets),
            },
            repo_origin_dir: StrTabRef {
                bytes: &$d.repo_origin_dir_bytes,
                offsets: $u32s(&$d.repo_origin_dir_offsets),
            },
            repo_origin_evidence: StrTabRef {
                bytes: &$d.repo_origin_evidence_bytes,
                offsets: $u32s(&$d.repo_origin_evidence_offsets),
            },
            repo_confirmed: &$d.repo_confirmed,
            repo_indexed: &$d.repo_indexed,
            repo_input_patterns: StrTabRef {
                bytes: &$d.repo_input_bytes,
                offsets: $u32s(&$d.repo_input_offsets),
            },
            repo_input_prefix: &$d.repo_input_prefix,
            repo_input_target_only: &$d.repo_input_target_only,
            repo_inputs: CsrRef {
                offsets: $u32s(&$d.repo_input_repo_offsets),
                edges: $u32s(&$d.repo_input_repo_edges),
            },
            bzl_repos: CsrRef {
                offsets: $u32s(&$d.bzl_repo_offsets),
                edges: $u32s(&$d.bzl_repo_edges),
            },
            repo_maps: StrTabRef {
                bytes: &$d.repo_map_bytes,
                offsets: $u32s(&$d.repo_map_offsets),
            },
            repo_map_ranges: $u32s(&$d.repo_map_ranges),
            repo_consumers: StrTabRef {
                bytes: &$d.repo_consumer_bytes,
                offsets: $u32s(&$d.repo_consumer_offsets),
            },
            repo_consumer_ranges: $u32s(&$d.repo_consumer_ranges),
            repo_boundaries: StrTabRef {
                bytes: &$d.repo_boundary_bytes,
                offsets: $u32s(&$d.repo_boundary_offsets),
            },
            repo_boundary_ignored: &$d.repo_boundary_ignored,
            repo_boundary_traversed: &$d.repo_boundary_traversed,
            sky: crate::sky::SkyRef {
                present: $d.header.sky_layer,
                roots: StrTabRef {
                    bytes: &$d.sky_root_bytes,
                    offsets: $u32s(&$d.sky_root_offsets),
                },
                keys: StrTabRef {
                    bytes: &$d.sky_key_bytes,
                    offsets: $u32s(&$d.sky_key_offsets),
                },
                kind: &$d.sky_kind,
                root: $u32s(&$d.sky_root),
                rdeps: CsrRef {
                    offsets: $u32s(&$d.sky_rdep_offsets),
                    edges: $u32s(&$d.sky_rdep_edges),
                },
            },
        }
    };
}

/// The `$u32s` adapter for the owned struct: its arrays are already `[u32]`.
#[inline]
fn as_u32s(v: &[u32]) -> &[u32] {
    v
}

impl IndexData {
    pub fn view(&self) -> IndexRef<'_> {
        index_view!(self, self.provenance, as_u32s, self.header.format_version)
    }
}

impl ArchivedIndexData {
    /// A view over the archive. Provenance is not in the archive — it comes
    /// from the file's trailer — so the caller supplies it.
    pub fn view_with<'a>(&'a self, provenance: &'a Provenance) -> IndexRef<'a> {
        index_view!(
            self,
            provenance,
            le::u32s,
            self.header.format_version.into()
        )
    }
}
