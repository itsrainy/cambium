//! The Skyframe loading layer: Bazel's own loading-phase graph, stored.
//!
//! The rule layer above this one (targets, rule edges, rule hashes, file
//! consumers) answers "what depends on this target". It cannot answer "which
//! packages did this changed *path* move", because that question is about
//! sub-package boundaries, `glob` results and `load()` chains, none of which
//! are targets. cambium used to answer it with three hand-written models: a
//! carving rule for `BUILD` adds and deletes, a glob rule for adds and deletes
//! of ordinary files, and a statically parsed `.bzl` load closure. Each was a
//! restatement of something Bazel already computes exactly.
//!
//! This layer is that computation, read back out of `bazel dump
//! --skyframe=deps` and stored as reverse edges. A changed path becomes the
//! Skyframe keys the change moved ([`seeds`]), the walk follows reverse
//! dependencies, and every `PACKAGE` or `PACKAGE_LOOKUP` it reaches is a dirty
//! package. There is no case analysis in it at all.
//!
//! # The kinds, and why these
//!
//! `FILE_STATE` and `DIRECTORY_LISTING_STATE` are the leaves a diff can name.
//! `FILE` and `DIRECTORY_LISTING` are the hops above them. `GLOB`/`GLOBS`
//! carries a package's glob results, `PACKAGE_LOOKUP` whether a directory is a
//! package, `PACKAGE` the loaded package, `BZL_LOAD` one Starlark file's load
//! (and `BZL_LOAD -> BZL_LOAD` its transitive loads).
//! `IGNORED_SUBDIRECTORIES` (Bazel 8/9) and `IGNORED_PACKAGE_PREFIXES`
//! (Bazel 7) are the one singleton kept: every package depends on it and it
//! depends on `.bazelignore`, so keeping it is exactly the statement that
//! editing `.bazelignore` moves every package. Nothing else seeds into it.
//!
//! Every other kind in the dump is dropped. `PRECOMPUTED:*`,
//! `STARLARK_BUILTINS` and `REPOSITORY_MAPPING` are singletons every package
//! depends on that no changed path reaches, so keeping them would put every
//! package one reverse hop from every other and the walk would answer `//...`
//! on every change.
//!
//! # Spellings
//!
//! Verified against real dumps on Bazel 7.7.0, 8.7.0 and 9.2.0:
//!
//! ```text
//! FILE_STATE:[/abs/root]/[rel/path]
//! DIRECTORY_LISTING:[/abs/root]/[rel/dir]
//! PACKAGE:pkg                       PACKAGE_LOOKUP:pkg   (root package: empty)
//! BZL_LOAD:KeyForBuild{label=@@repo//pkg:f.bzl, isBuildPrelude=false}
//! GLOBS:<GlobsKey packageRoot = /abs, packageIdentifier = pkg, globRequests = [...]>   (8/9)
//! GLOB:<GlobDescriptor packageName=pkg packageRoot=/abs subdir= pattern=*.txt globberOperation=FILES>  (7)
//! IGNORED_SUBDIRECTORIES:@@         (8/9)   IGNORED_PACKAGE_PREFIXES:@@   (7)
//! ```
//!
//! A rooted key is stored as (root id, relative path) with the workspace as
//! root 0, so no absolute path of the machine that built the index is in the
//! payload. An unrooted key embeds absolute paths in its own text instead, and
//! those are replaced with `<workspace>` and `<output_base>` placeholders for
//! the same reason.

pub mod dump;

use std::collections::{BTreeSet, HashMap, VecDeque};

use crate::csr::{Csr, CsrRef};
use crate::strtab::StrTabRef;

/// A Skyframe node kind the layer keeps. The discriminants are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Kind {
    FileState = 0,
    File = 1,
    DirectoryListingState = 2,
    DirectoryListing = 3,
    /// `GLOBS` on Bazel 8/9, `GLOB` on Bazel 7. One kind: the two spell the
    /// same edge, and only the key's own text differs.
    Glob = 4,
    PackageLookup = 5,
    Package = 6,
    BzlLoad = 7,
    /// `IGNORED_SUBDIRECTORIES` on Bazel 8/9, `IGNORED_PACKAGE_PREFIXES` on
    /// Bazel 7.
    Ignored = 8,
    /// The node named something that no longer exists: a package this run
    /// deleted. Kept rather than removed because ids are stable across a
    /// splice, exactly as a tombstoned target node is. It seeds nothing and
    /// names no package.
    Tombstone = 9,
}

/// Highest discriminant, for reading a stored byte back.
const MAX_KIND: u8 = Kind::Tombstone as u8;

impl Kind {
    /// The kind a dump key names, or `None` for a kind the layer drops.
    pub fn from_dump(kind: &str) -> Option<Self> {
        Some(match kind {
            "FILE_STATE" => Kind::FileState,
            "FILE" => Kind::File,
            "DIRECTORY_LISTING_STATE" => Kind::DirectoryListingState,
            "DIRECTORY_LISTING" => Kind::DirectoryListing,
            "GLOBS" | "GLOB" => Kind::Glob,
            "PACKAGE_LOOKUP" => Kind::PackageLookup,
            "PACKAGE" => Kind::Package,
            "BZL_LOAD" => Kind::BzlLoad,
            "IGNORED_SUBDIRECTORIES" | "IGNORED_PACKAGE_PREFIXES" => Kind::Ignored,
            _ => return None,
        })
    }

    #[inline]
    pub fn from_u8(v: u8) -> Option<Self> {
        (v <= MAX_KIND).then(|| {
            // SAFETY: `Kind` is `repr(u8)` with contiguous discriminants
            // `0..=MAX_KIND`, and `v` was just checked to be in that range.
            unsafe { std::mem::transmute::<u8, Kind>(v) }
        })
    }

    /// Whether this kind's key is written as `[root]/[relative/path]`.
    #[inline]
    pub fn is_rooted(self) -> bool {
        matches!(
            self,
            Kind::FileState | Kind::File | Kind::DirectoryListingState | Kind::DirectoryListing
        )
    }

    /// Whether reaching this node means a package must be re-queried.
    #[inline]
    pub fn is_package(self) -> bool {
        matches!(self, Kind::Package | Kind::PackageLookup)
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::FileState => "FILE_STATE",
            Kind::File => "FILE",
            Kind::DirectoryListingState => "DIRECTORY_LISTING_STATE",
            Kind::DirectoryListing => "DIRECTORY_LISTING",
            Kind::Glob => "GLOB",
            Kind::PackageLookup => "PACKAGE_LOOKUP",
            Kind::Package => "PACKAGE",
            Kind::BzlLoad => "BZL_LOAD",
            Kind::Ignored => "IGNORED",
            Kind::Tombstone => "TOMBSTONE",
        }
    }
}

/// The workspace root's id. Always 0, and always the empty string, so a stored
/// layer names no absolute path of the machine that built it.
pub const WORKSPACE: u32 = 0;

/// The placeholder an unrooted key's embedded workspace path becomes.
pub const WORKSPACE_MARK: &str = "<workspace>";
/// The placeholder an unrooted key's embedded output base becomes.
pub const OUTPUT_BASE_MARK: &str = "<output_base>";

/// The `--skykey_filter` the dump is taken with.
///
/// A prefix set rather than an exact list, because `FILE` matches `FILE_STATE`
/// and `PACKAGE` matches `PACKAGE_LOOKUP`, and because dropping either half of
/// a pair cuts every chain through it in two. `GLOB` covers Bazel 7's `GLOB`
/// and 8/9's `GLOBS` alike, `IGNORED_` both spellings of the ignore node.
pub const SKYKEY_FILTER: &str = "^(PACKAGE|GLOB|FILE|DIRECTORY_LISTING|BZL_LOAD|IGNORED_)";

/// Which absolute paths to erase from an unrooted key's own text.
#[derive(Clone, Debug, Default)]
pub struct Paths {
    /// The Bazel workspace, absolute and without a trailing slash.
    pub workspace: String,
    /// The server's output base, where external repositories live. Empty when
    /// the caller could not name it, which costs only that those keys keep a
    /// machine-specific spelling.
    pub output_base: String,
}

impl Paths {
    pub fn new(workspace: impl AsRef<str>, output_base: impl AsRef<str>) -> Self {
        Self {
            workspace: workspace.as_ref().trim_end_matches('/').to_string(),
            output_base: output_base.as_ref().trim_end_matches('/').to_string(),
        }
    }
}

/// One node of the layer, as the owned form holds it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SkyNode {
    pub kind: Kind,
    pub root: u32,
    /// The relative path for a rooted kind, the normalised key text otherwise.
    pub text: String,
}

/// The owned, writable layer: what a build or a splice assembles before the
/// index is finished.
///
/// Edges are kept forward (a node to what it depends on) because that is how
/// the dump states them and how a refresh replaces one node's row; the reverse
/// CSR the walk reads is computed once, at [`SkyLayer::finish`].
#[derive(Clone, Debug, Default)]
pub struct SkyLayer {
    roots: Vec<String>,
    root_ids: HashMap<String, u32>,
    nodes: Vec<SkyNode>,
    ids: HashMap<SkyNode, u32>,
    fwd: Vec<Vec<u32>>,
}

impl SkyLayer {
    pub fn new() -> Self {
        Self {
            roots: vec![String::new()],
            ..Default::default()
        }
    }

    pub fn num_nodes(&self) -> usize {
        self.nodes.len()
    }

    pub fn num_edges(&self) -> usize {
        self.fwd.iter().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Reads a stored layer back, inverting its reverse edges.
    ///
    /// O(edges), which at a few hundred thousand is a couple of milliseconds,
    /// and it is paid only by a run that materialises a new index.
    pub fn from_view(v: SkyRef<'_>) -> Option<Self> {
        if !v.present {
            return None;
        }
        let mut out = Self::new();
        out.roots = v.roots.iter().map(str::to_string).collect();
        if out.roots.is_empty() {
            out.roots.push(String::new());
        }
        for (i, r) in out.roots.iter().enumerate() {
            out.root_ids.insert(r.clone(), i as u32);
        }
        out.nodes = (0..v.len() as u32).map(|i| v.node(i)).collect();
        for (i, n) in out.nodes.iter().enumerate() {
            out.ids.insert(n.clone(), i as u32);
        }
        out.fwd = vec![Vec::new(); out.nodes.len()];
        for to in 0..v.len() as u32 {
            for &from in v.rdeps.row(to) {
                out.fwd[from as usize].push(to);
            }
        }
        Some(out)
    }

    fn root_id(&mut self, root: &str, paths: &Paths) -> u32 {
        let root = root.trim_end_matches('/');
        if !paths.workspace.is_empty() && root == paths.workspace {
            return WORKSPACE;
        }
        let stored = erase(root, paths);
        if let Some(&id) = self.root_ids.get(&stored) {
            return id;
        }
        let id = self.roots.len() as u32;
        self.roots.push(stored.clone());
        self.root_ids.insert(stored, id);
        id
    }

    /// The node a dump key names, or `None` for a kind the layer drops.
    fn normalise(&mut self, key: &str, paths: &Paths) -> Option<SkyNode> {
        let (kind_str, rest) = dump::kind_of(key)?;
        let kind = Kind::from_dump(kind_str)?;
        if kind.is_rooted() {
            let (root, rel) = dump::split_rooted(rest)?;
            let root = self.root_id(root, paths);
            return Some(SkyNode {
                kind,
                root,
                text: rel.to_string(),
            });
        }
        Some(SkyNode {
            kind,
            root: WORKSPACE,
            text: erase(rest, paths),
        })
    }

    fn intern(&mut self, node: SkyNode) -> u32 {
        if let Some(&id) = self.ids.get(&node) {
            return id;
        }
        let id = self.nodes.len() as u32;
        self.nodes.push(node.clone());
        self.fwd.push(Vec::new());
        self.ids.insert(node, id);
        id
    }

    pub fn id_of(&self, node: &SkyNode) -> Option<u32> {
        self.ids.get(node).copied()
    }

    /// Adds every block of one dump, replacing the dependency row of any node
    /// the dump names.
    ///
    /// Replacing rather than merging is what makes a *filtered* dump a refresh:
    /// the incremental run re-dumps only the keys of the packages it
    /// re-queried, and a glob that no longer reaches a directory must lose that
    /// edge, not keep it. A node the dump does not name keeps the row it had,
    /// which is the whole reason the filtered dump is cheap.
    ///
    /// Returns how many blocks the dump held, kept or not.
    pub fn apply_dump(&mut self, text: &str, paths: &Paths) -> usize {
        // The sink borrows the text while `self` is borrowed mutably, so the
        // blocks are collected first. A full dump of the largest subject is a
        // few tens of megabytes of text.
        let mut blocks: Vec<(String, Vec<String>)> = Vec::new();
        dump::parse_dump(text, &mut |k, d| {
            blocks.push((k.to_string(), d.iter().map(|s| s.to_string()).collect()))
        });
        let seen = blocks.len();
        for (key, deps) in blocks {
            let Some(node) = self.normalise(&key, paths) else {
                continue;
            };
            let id = self.intern(node);
            let mut row: Vec<u32> = Vec::with_capacity(deps.len());
            for dep in deps {
                let Some(d) = self.normalise(&dep, paths) else {
                    continue;
                };
                row.push(self.intern(d));
            }
            row.sort_unstable();
            row.dedup();
            self.fwd[id as usize] = row;
        }
        seen
    }

    /// Retires the layer's rows for a package the run deleted: its `PACKAGE`
    /// node stops naming a package and stops depending on anything, so nothing
    /// reaches it and it dirties nothing.
    ///
    /// Its `PACKAGE_LOOKUP` stays exactly as it is. A directory that stopped
    /// being a package is still a directory Bazel looks in, and the lookup is
    /// what a later commit re-adding the `BUILD` file will move.
    pub fn delete_package(&mut self, package: &str) {
        let node = SkyNode {
            kind: Kind::Package,
            root: WORKSPACE,
            text: package.to_string(),
        };
        let Some(&id) = self.ids.get(&node) else {
            return;
        };
        self.fwd[id as usize].clear();
        self.nodes[id as usize].kind = Kind::Tombstone;
        // The id keeps its old entry so a later `apply_dump` of the same key
        // re-interns to a *new* id rather than resurrecting the tombstone with
        // stale edges pointing at it.
        self.ids.remove(&node);
    }

    /// Freezes the layer into the arrays the index stores: the node table,
    /// ordered by `(kind, root, text)` so a seed can be found by binary
    /// search, and the reverse edges as a CSR over that order.
    pub fn finish(self) -> SkyTables {
        let n = self.nodes.len();
        // Roots first. A root id is part of every node's sort key, so leaving
        // the root table in encounter order would make the stored node order
        // depend on the order the dump's blocks arrived in. The workspace keeps
        // id 0; the rest go in name order.
        let mut root_order: Vec<u32> = (1..self.roots.len() as u32).collect();
        root_order.sort_unstable_by(|&a, &b| self.roots[a as usize].cmp(&self.roots[b as usize]));
        let mut root_remap = vec![WORKSPACE; self.roots.len()];
        let mut roots = vec![self.roots[WORKSPACE as usize].clone()];
        for (new, &old) in root_order.iter().enumerate() {
            root_remap[old as usize] = new as u32 + 1;
            roots.push(self.roots[old as usize].clone());
        }
        let mut nodes = self.nodes;
        for node in &mut nodes {
            node.root = root_remap[node.root as usize];
        }

        // Sort the nodes rather than the ids: the stored order *is* the id
        // space, so the payload is a function of the layer's content and not
        // of the order the dump's blocks happened to arrive in.
        let mut order: Vec<u32> = (0..n as u32).collect();
        order.sort_unstable_by(|&a, &b| nodes[a as usize].cmp(&nodes[b as usize]));
        let mut remap = vec![0u32; n];
        for (new, &old) in order.iter().enumerate() {
            remap[old as usize] = new as u32;
        }

        let mut t = SkyTables {
            present: true,
            ..SkyTables::empty()
        };
        for r in &roots {
            t.root_bytes.extend_from_slice(r.as_bytes());
            t.root_offsets.push(t.root_bytes.len() as u32);
        }
        t.kind = Vec::with_capacity(n);
        t.root = Vec::with_capacity(n);
        for &old in &order {
            let node = &nodes[old as usize];
            t.kind.push(node.kind as u8);
            t.root.push(node.root);
            t.key_bytes.extend_from_slice(node.text.as_bytes());
            t.key_offsets.push(t.key_bytes.len() as u32);
        }

        // Reverse edges, counted then filled: one pass each, no per-node Vec.
        let mut offsets = vec![0u32; n + 1];
        for row in &self.fwd {
            for &d in row {
                offsets[remap[d as usize] as usize + 1] += 1;
            }
        }
        for i in 1..offsets.len() {
            offsets[i] += offsets[i - 1];
        }
        let mut cursor = offsets.clone();
        let mut edges = vec![0u32; offsets[n] as usize];
        for (from, row) in self.fwd.iter().enumerate() {
            for &d in row {
                let slot = &mut cursor[remap[d as usize] as usize];
                edges[*slot as usize] = remap[from];
                *slot += 1;
            }
        }
        // CSR rows are sorted and deduplicated everywhere else in the crate,
        // and the rebuild oracle compares them row for row.
        for i in 0..n {
            let (s, e) = (offsets[i] as usize, offsets[i + 1] as usize);
            edges[s..e].sort_unstable();
        }
        t.rdep_offsets = offsets;
        t.rdep_edges = edges;
        t
    }
}

/// Replaces the absolute paths an unrooted key embeds with placeholders.
fn erase(text: &str, paths: &Paths) -> String {
    let mut out = text.to_string();
    // The output base first: on a default layout it sits *inside* nothing the
    // workspace names, but a caller may have put it under the workspace, and
    // erasing the longer path first keeps that case stable.
    if !paths.output_base.is_empty() && out.contains(&paths.output_base) {
        out = out.replace(&paths.output_base, OUTPUT_BASE_MARK);
    }
    if !paths.workspace.is_empty() && out.contains(&paths.workspace) {
        out = out.replace(&paths.workspace, WORKSPACE_MARK);
    }
    out
}

/// The layer's stored arrays, as [`crate::IndexData`] holds them.
#[derive(Clone, Debug, Default)]
pub struct SkyTables {
    pub present: bool,
    pub root_bytes: Vec<u8>,
    pub root_offsets: Vec<u32>,
    pub key_bytes: Vec<u8>,
    pub key_offsets: Vec<u32>,
    pub kind: Vec<u8>,
    pub root: Vec<u32>,
    pub rdep_offsets: Vec<u32>,
    pub rdep_edges: Vec<u32>,
}

impl SkyTables {
    pub fn empty() -> Self {
        Self {
            present: false,
            root_bytes: Vec::new(),
            root_offsets: vec![0],
            key_bytes: Vec::new(),
            key_offsets: vec![0],
            kind: Vec::new(),
            root: Vec::new(),
            rdep_offsets: vec![0],
            rdep_edges: Vec::new(),
        }
    }

    /// A view over the owned tables, for a caller holding them outside an
    /// index (the builder's own tests, and the rebuild oracle's diff).
    pub fn as_ref(&self) -> SkyRef<'_> {
        SkyRef {
            present: self.present,
            roots: StrTabRef {
                bytes: &self.root_bytes,
                offsets: &self.root_offsets,
            },
            keys: StrTabRef {
                bytes: &self.key_bytes,
                offsets: &self.key_offsets,
            },
            kind: &self.kind,
            root: &self.root,
            rdeps: CsrRef {
                offsets: &self.rdep_offsets,
                edges: &self.rdep_edges,
            },
        }
    }
}

/// Borrowed view over a stored layer.
#[derive(Clone, Copy)]
pub struct SkyRef<'a> {
    /// Whether the layer means anything.
    ///
    /// Recorded rather than inferred from an empty table, for the same reason
    /// the `.bzl` closure it replaces recorded it: "Bazel's loading phase
    /// touched nothing" and "this index was built without a dump" are the same
    /// empty arrays, and only one of them is safe to answer a change from.
    pub present: bool,
    pub roots: StrTabRef<'a>,
    /// The node table's keys, one per node, in `(kind, root, text)` order.
    pub keys: StrTabRef<'a>,
    pub kind: &'a [u8],
    pub root: &'a [u32],
    /// For each node, the nodes that depend on it.
    pub rdeps: CsrRef<'a>,
}

impl<'a> SkyRef<'a> {
    pub const EMPTY: SkyRef<'static> = SkyRef {
        present: false,
        roots: StrTabRef {
            bytes: &[],
            offsets: &[0],
        },
        keys: StrTabRef {
            bytes: &[],
            offsets: &[0],
        },
        kind: &[],
        root: &[],
        rdeps: CsrRef::EMPTY,
    };

    #[inline]
    pub fn len(&self) -> usize {
        self.kind.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.kind.is_empty()
    }

    #[inline]
    pub fn num_edges(&self) -> usize {
        self.rdeps.num_edges()
    }

    #[inline]
    pub fn kind_of(&self, id: u32) -> Kind {
        Kind::from_u8(self.kind[id as usize]).unwrap_or(Kind::Tombstone)
    }

    #[inline]
    pub fn text(&self, id: u32) -> &'a str {
        self.keys.get(id)
    }

    pub fn node(&self, id: u32) -> SkyNode {
        SkyNode {
            kind: self.kind_of(id),
            root: self.root[id as usize],
            text: self.text(id).to_string(),
        }
    }

    /// How many nodes of each kind the layer holds, for `index inspect`.
    pub fn counts(&self) -> Vec<(Kind, usize)> {
        let mut out: Vec<(Kind, usize)> = Vec::new();
        for id in 0..self.len() as u32 {
            let k = self.kind_of(id);
            match out.iter_mut().find(|(kk, _)| *kk == k) {
                Some((_, n)) => *n += 1,
                None => out.push((k, 1)),
            }
        }
        out.sort_by_key(|(k, _)| *k as u8);
        out
    }

    /// The id of a node under the workspace root. O(log n): the table is in
    /// `(kind, root, text)` order.
    pub fn id_of(&self, kind: Kind, text: &str) -> Option<u32> {
        let key = (kind as u8, WORKSPACE, text);
        let mut lo = 0usize;
        let mut hi = self.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let probe = (
                self.kind[mid],
                self.root[mid],
                self.keys.get(mid as u32),
            );
            match probe.cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid as u32),
            }
        }
        None
    }

    /// The generic dirty stage: seed, walk reverse edges, collect packages.
    ///
    /// There is no case analysis here, and that is the point. What a change
    /// means is entirely in `seeds`, and what the change reaches is entirely
    /// in the edges Bazel itself recorded.
    pub fn walk(&self, seeds: &BTreeSet<(Kind, String)>) -> SkyWalk {
        let mut out = SkyWalk::default();
        if !self.present {
            return out;
        }
        let mut seen = vec![false; self.len()];
        let mut queue: VecDeque<u32> = VecDeque::new();
        for (kind, text) in seeds {
            match self.id_of(*kind, text) {
                Some(id) => {
                    if !seen[id as usize] {
                        seen[id as usize] = true;
                        queue.push_back(id);
                    }
                }
                // Normal, and counted rather than guessed at: a path Bazel's
                // loading phase never looked at has no node, and reaches
                // nothing through this layer.
                None => out.unknown_seeds += 1,
            }
        }
        while let Some(id) = queue.pop_front() {
            out.visited += 1;
            let kind = self.kind_of(id);
            if kind.is_package() {
                let text = self.text(id);
                // A package of some *other* repository: `@@rules_cc+//pkg`.
                // Nothing in a main-repository diff re-queries it, and the
                // externals stage is what routes a change that moved it.
                if is_main_package(text) {
                    if kind == Kind::Package {
                        out.packages.insert(text.to_string());
                    } else {
                        out.lookups.insert(text.to_string());
                    }
                } else {
                    out.external_packages += 1;
                }
            }
            for &up in self.rdeps.row(id) {
                if !seen[up as usize] {
                    seen[up as usize] = true;
                    queue.push_back(up);
                }
            }
        }
        out
    }
}

/// Whether a `PACKAGE`/`PACKAGE_LOOKUP` key names a package of the main
/// repository. External ones carry their repository: `@@rules_java//java`.
fn is_main_package(text: &str) -> bool {
    !text.starts_with('@') && !text.contains("//")
}

/// What one reverse walk reached.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkyWalk {
    /// Main-repository packages that were loaded and are now dirty.
    pub packages: BTreeSet<String>,
    /// Directories whose *lookup* the change moved without a loaded package
    /// behind it: a package this commit creates, and a directory that never
    /// held one. Both are re-queried; the second comes back empty and
    /// tombstones nothing, because there was nothing there.
    pub lookups: BTreeSet<String>,
    /// Packages of another repository the walk reached, counted for the
    /// report. The external stage is what acts on those.
    pub external_packages: usize,
    /// Seeds that named no node in the layer.
    pub unknown_seeds: usize,
    /// How many nodes the walk visited, for the cost line.
    pub visited: usize,
}

impl SkyWalk {
    /// Every directory the walk says must be re-queried.
    pub fn dirty(&self) -> impl Iterator<Item = &str> {
        self.packages
            .iter()
            .chain(self.lookups.iter())
            .map(String::as_str)
    }
}

/// The Skyframe keys a set of changed paths moved.
///
/// Every changed path is a `FILE_STATE`. A path that was added or deleted also
/// changes the *listing* of the directory holding it and of every ancestor
/// (an add may have created the whole chain, a delete may have emptied it).
/// A content-only edit moves neither.
///
/// The ancestor directories' own `FILE_STATE` is deliberately *not* seeded.
/// A directory's file state says "this is a directory", and that does not move
/// when a file appears beside it; seeding it anyway walks the `FILE` parent
/// chain (`FILE:[a/BUILD] -> FILE:[a]`) and dirties every package above the
/// change, which is the kind of over-approximation this layer exists to
/// retire.
///
/// A changed `.bzl` needs no case of its own. Its `BZL_LOAD` node depends on
/// the `FILE` node of the file, which depends on its `FILE_STATE`, so the
/// ordinary seed reaches it, and `BZL_LOAD -> BZL_LOAD` carries it through a
/// `.bzl` loaded only by other `.bzl` files.
pub fn seeds<'a>(
    changed: impl IntoIterator<Item = (&'a str, bool)>,
) -> BTreeSet<(Kind, String)> {
    let mut out = BTreeSet::new();
    for (path, structural) in changed {
        out.insert((Kind::FileState, path.to_string()));
        if !structural {
            continue;
        }
        let mut dir = path;
        loop {
            dir = match dir.rfind('/') {
                Some(i) => &dir[..i],
                // The workspace root's own listing, spelled as the empty path
                // exactly as `DIRECTORY_LISTING_STATE:[/ws]/[]` is.
                None => {
                    out.insert((Kind::DirectoryListingState, String::new()));
                    break;
                }
            };
            out.insert((Kind::DirectoryListingState, dir.to_string()));
        }
    }
    out
}


#[cfg(test)]
mod tests;
