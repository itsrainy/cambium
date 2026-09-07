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
        }
    }
}
