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
//!
//! # A dump can come back without its package values
//!
//! A server that has just answered `//...:*` can hold every `PACKAGE` node
//! without a value: `dump --skyframe=count` reports 644 packages and
//! `dump --skyframe=deps`, seconds later on that same server, prints one. The
//! dump still carries every `FILE_STATE`, every `PACKAGE_LOOKUP` and every
//! glob, so neither its size nor its node counts show the loss, and the walk
//! over the layer that results answers "two packages" for a commit that moved
//! six hundred. Asking Bazel for the same packages once more materialises
//! them, and the dump that follows is whole. Seen on apollo (7.4.1), reported
//! on xla and envoy (8.7).
//!
//! The cause is what ran on that server in between. A Bazel command whose
//! option set differs from the last one's re-evaluates, and a command that
//! runs its own Skyframe evaluation drops what it no longer needs. Two such
//! commands used to sit between the universe query and the dump, both from
//! the closure rescan: `mod dump_repo_mapping` and
//! `query //external:* --output=build`. Bisected on apollo, one at a time on
//! a fresh output base, each costs the dump 19 of its 644 packages, and
//! through the engine the two together cost it all but one. So the dump runs
//! first now, on the cold build and on the splice alike.
//!
//! What it is not, each ruled out against the same reproducer: the Skyframe
//! high-water-mark GC (`--skyframe_high_water_mark_threshold=100` changes
//! nothing), `--order_output=no` streaming (`--order_output=full` changes
//! nothing), and the reader closing the proto pipe before EOF (draining and
//! closing after 200 KB both keep all 644).
//!
//! The layer is verified anyway, because the next command added between the
//! query and the dump would be just as quiet: [`SkyLayer::looks_loaded`] on a
//! cold build and [`SkyLayer::refresh_shortfall`] on a splice, each repaired
//! by loading the packages again and dumping once more, and each an error if
//! that does not help.

pub mod dump;

use std::collections::{BTreeSet, HashMap, VecDeque};

use crate::csr::CsrRef;
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

/// Not a root id: the answer for a key under a root that is not the
/// workspace, which the layer drops.
const NOT_MINE: u32 = u32::MAX;

/// The placeholder an unrooted key's embedded workspace path becomes.
pub const WORKSPACE_MARK: &str = "<workspace>";
/// The placeholder an unrooted key's embedded output base becomes.
pub const OUTPUT_BASE_MARK: &str = "<output_base>";

/// The `--skykey_filter` a cold build's dump is taken with.
///
/// A prefix set rather than an exact list, because `FILE` matches `FILE_STATE`
/// and `PACKAGE` matches `PACKAGE_LOOKUP`, and because dropping either half of
/// a pair cuts every chain through it in two. `GLOB` covers Bazel 7's `GLOB`
/// and 8/9's `GLOBS` alike, `IGNORED_` both spellings of the ignore node.
pub const SKYKEY_FILTER: &str = "^(PACKAGE|GLOB|FILE|DIRECTORY_LISTING|BZL_LOAD|IGNORED_)";

/// Past this many packages a refresh asks for the whole loading phase instead.
///
/// The filter is one command-line argument and grows with the package list
/// (about 260 bytes a package), so a very large dirty set would build a
/// megabyte of regex to save less than dumping everything costs. A run that
/// dirty is re-querying most of the repository anyway.
pub const MAX_FILTERED_PACKAGES: usize = 500;

/// The `--skykey_filter` for a refresh after a run re-queried `packages`.
///
/// A refresh has to leave the layer exactly as a cold build at the same commit
/// would: the rebuild oracle compares them row for row, and a row the refresh
/// failed to write is a change that will not reach a package later. So the
/// filter is not "the packages' own keys" but everything re-querying them can
/// have moved:
///
/// - `PACKAGE` and the glob nodes of those packages, which is what changed;
/// - every key *under* one of them, because a package can gain a
///   sub-directory, a `BUILD` file or a listing, and those nodes have to exist
///   before a later commit can seed them;
/// - every `BZL_LOAD`, and the `FILE`/`FILE_STATE` of every `.bzl`, because a
///   Starlark file's own loads move without its package being re-queried, and
///   a new `.bzl` anywhere needs its file chain;
/// - the ignore node, which is one key.
///
/// Everything else keeps the row it has: [`SkyLayer::apply_dump`] replaces
/// only the rows of keys the dump names, so a filtered dump is an upsert over
/// the packages it covers rather than a new layer.
///
/// Returns `None` for an empty package list. A run that re-queried nothing
/// moved no loading-phase node and needs no dump at all, which is the
/// difference between a step costing milliseconds and a step costing a
/// traversal of the whole graph.
pub fn refresh_filter(packages: &[String]) -> Option<String> {
    if packages.is_empty() || packages.len() > MAX_FILTERED_PACKAGES {
        return None;
    }
    // A package name's characters, so a glob key's package can be delimited
    // without writing a comma (the flag is a comma-separated list, so a comma
    // inside a pattern would split it).
    const NOT_PACKAGE_CHAR: &str = "[^A-Za-z0-9/._+-]";
    let names: Vec<String> = packages.iter().map(|p| escape(p)).collect();
    // The subtree of each package: the package itself, or anything below it.
    // The root package's subtree is the whole tree, which is what a cold dump
    // holds anyway.
    let subtree: Vec<String> = packages
        .iter()
        .map(|p| {
            if p.is_empty() {
                "[^]]*".to_string()
            } else {
                format!("{}(/[^]]*)?", escape(p))
            }
        })
        .collect();
    let p = names.join("|");
    let q = subtree.join("|");
    Some(format!(
        "^(PACKAGE:({p})$\
         |PACKAGE_LOOKUP:({q})$\
         |GLOBS?:.*(packageIdentifier = ({p}){NOT_PACKAGE_CHAR}|packageName=({p}){NOT_PACKAGE_CHAR})\
         |BZL_LOAD:\
         |IGNORED_\
         |(FILE|FILE_STATE):{ROOTED}{BZL}\
         |(FILE|FILE_STATE|DIRECTORY_LISTING|DIRECTORY_LISTING_STATE):{ROOTED}({q}){CLOSE})",
        ROOTED = ROOTED_PREFIX,
        BZL = BZL_SUFFIX,
        CLOSE = CLOSE_SUFFIX,
    ))
}

/// `[root]/[` as a regex: the opening of every rooted key.
const ROOTED_PREFIX: &str = r"\[[^]]*\]/\[";
/// The rest of a rooted key naming any `.bzl` file.
const BZL_SUFFIX: &str = r"[^]]*\.bzl\]$";
/// The rest of a rooted key whose relative path the caller has just matched.
const CLOSE_SUFFIX: &str = r"\]$";

/// A package name as a literal in a regex. Bazel allows `.`, `+` and `-` in a
/// package name, and the first two are metacharacters.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if "\\.+*?()[]{}^$|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Which absolute paths are this workspace's, and which to erase from an
/// unrooted key's own text.
///
/// Every spelling of the workspace root counts, because Bazel does not
/// necessarily write back the path it was handed: it resolves symbolic links
/// and prints the real path in `[root]`. On macOS that is not an edge case,
/// it is the default for anything under `$TMPDIR` (`/var` is a link to
/// `/private/var`), which is where a `cambi profile` worktree lives. Matching
/// on the given spelling alone dropped every `FILE`, `FILE_STATE`,
/// `DIRECTORY_LISTING` and `GLOB` key as "another repository's", leaving a
/// layer that no changed path could reach a package through, and saying
/// nothing about it.
#[derive(Clone, Debug, Default)]
pub struct Paths {
    /// Spellings of the Bazel workspace root, without trailing slashes.
    workspaces: Vec<String>,
    /// Spellings of the server's output base, where external repositories
    /// live. Empty when the caller could not name it, which costs only that
    /// those keys keep a machine-specific spelling.
    output_bases: Vec<String>,
}

impl Paths {
    /// `workspace` and `output_base` as the caller knows them. Each is also
    /// resolved through the filesystem, and both spellings are kept: the
    /// resolved one is what Bazel prints, the given one is what a caller with
    /// no such directory on disk (a test, a synthetic subject) means.
    pub fn new(workspace: impl AsRef<str>, output_base: impl AsRef<str>) -> Self {
        Self {
            workspaces: spellings(workspace.as_ref()),
            output_bases: spellings(output_base.as_ref()),
        }
    }

    /// Whether a dump's `[root]` names this workspace, in any of its
    /// spellings.
    pub fn is_workspace(&self, root: &str) -> bool {
        let root = root.trim_end_matches('/');
        self.workspaces.iter().any(|w| w == root)
    }

    /// The workspace as the caller named it, for a message.
    pub fn workspace(&self) -> &str {
        self.workspaces.first().map_or("", String::as_str)
    }
}

/// A path, and its resolved form when that is a different string. Empty in,
/// empty out: a caller that could not name the directory names none.
fn spellings(path: &str) -> Vec<String> {
    let given = path.trim_end_matches('/').to_string();
    if given.is_empty() {
        return Vec::new();
    }
    let mut out = vec![given.clone()];
    if let Ok(real) = std::fs::canonicalize(&given) {
        let real = real.to_string_lossy().trim_end_matches('/').to_string();
        if real != given {
            out.push(real);
        }
    }
    out
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
    nodes: Vec<SkyNode>,
    ids: HashMap<SkyNode, u32>,
    fwd: Vec<Vec<u32>>,
    /// Roots the dump used that are not this workspace, for diagnosis. Small:
    /// one entry per external root a server holds.
    foreign_roots: BTreeSet<String>,
}

impl SkyLayer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn num_nodes(&self) -> usize {
        self.nodes.len()
    }

    pub fn num_edges(&self) -> usize {
        self.fwd.iter().map(Vec::len).sum()
    }

    /// How many nodes of one kind the layer holds.
    pub fn count_of(&self, kind: Kind) -> usize {
        self.nodes.iter().filter(|n| n.kind == kind).count()
    }

    /// The roots the dumps used that were not this workspace's.
    pub fn foreign_roots(&self) -> impl Iterator<Item = &str> {
        self.foreign_roots.iter().map(String::as_str)
    }

    /// How many main-repository packages the layer holds, and how many of
    /// those have at least one loading-phase dependency it kept: a
    /// `BZL_LOAD`, a `GLOB`, or the `PACKAGE_LOOKUP` of a sub-directory.
    ///
    /// The ratio is what tells a layer that describes the loading phase from
    /// one that merely has the right node *count*. Every kind can be present,
    /// every package can be there, and yet the edges that carry a changed
    /// `.bzl` or a moved directory listing to a package can be missing
    /// wholesale, because the keys they hang off were spelled in a way this
    /// build dropped. That is not a shortfall a walk notices: it answers, it
    /// answers quickly, and it answers too little.
    pub fn package_dep_health(&self) -> (usize, usize) {
        let mut total = 0usize;
        let mut healthy = 0usize;
        for (id, node) in self.nodes.iter().enumerate() {
            if node.kind != Kind::Package
                || !is_main_package(&node.text)
                || is_synthetic_package(&node.text)
            {
                continue;
            }
            total += 1;
            if self.fwd[id]
                .iter()
                .any(|&d| is_loading_dep(self.nodes[d as usize].kind))
            {
                healthy += 1;
            }
        }
        (total, healthy)
    }

    /// Whether this looks like the loading phase of a repository that was
    /// actually loaded, rather than its shape with the packages missing.
    ///
    /// The distinguishing fact is the ratio, not any count: a dump that lost
    /// its package values still holds every `PACKAGE_LOOKUP` (one per
    /// directory Bazel asked about), so a handful of packages under thousands
    /// of lookups is the signature. A repository really is allowed to have
    /// very few packages, which is why the test is against the lookups it
    /// performed and not against a number.
    pub fn looks_loaded(&self) -> bool {
        let lookups = self.count_of(Kind::PackageLookup);
        let (packages, with_deps) = self.package_dep_health();
        if lookups == 0 {
            // Nothing asked where a package was, so there is nothing to judge
            // the packages against. A filtered refresh looks like this.
            return true;
        }
        if packages == 0 {
            // Bazel looked for packages and the layer holds none of them.
            return false;
        }
        if lookups > 32 && packages * 32 < lookups {
            return false;
        }
        // No allowance is needed for `//external` any more: it is not counted
        // (see `is_synthetic_package`), and every package that is counted has
        // a lookup, a glob, a load or its own `BUILD` file.
        with_deps * 100 >= packages * 90
    }

    /// Whether any `BZL_LOAD` node has a `FILE` of this workspace under it.
    ///
    /// The chain a changed `.bzl` travels is `FILE_STATE -> FILE -> BZL_LOAD
    /// -> PACKAGE`. If no `BZL_LOAD` in the whole layer hangs off a file of
    /// this repository, that chain is cut for every Starlark file at once,
    /// and a `.bzl` edit reaches nothing.
    pub fn any_bzl_load_over_own_file(&self) -> bool {
        self.nodes.iter().enumerate().any(|(id, node)| {
            node.kind == Kind::BzlLoad
                && self.fwd[id].iter().any(|&d| {
                    let f = &self.nodes[d as usize];
                    f.kind == Kind::File && f.root == WORKSPACE
                })
        })
    }

    /// Whether this workspace has a Starlark file of its own in the layer.
    ///
    /// The question [`Self::any_bzl_load_over_own_file`] is only meaningful
    /// for: a repository whose every `.bzl` comes from somewhere else has no
    /// chain to cut, and its `BZL_LOAD` nodes hanging off nothing of its own
    /// is the truth rather than a broken dump. A workspace of only `BUILD`
    /// files and a `WORKSPACE` file is a real shape -- it is the shape of the
    /// smallest repository that consumes an external one -- and it used to
    /// fail the build outright.
    pub fn has_own_bzl_file(&self) -> bool {
        self.nodes.iter().any(|n| {
            matches!(n.kind, Kind::File | Kind::FileState)
                && n.root == WORKSPACE
                && n.text.ends_with(".bzl")
        })
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
        // A node under a root this build does not keep is dropped on the way
        // in, with the edges through it. Only the workspace root is
        // representable now, so a stored node under another one comes from an
        // index an earlier build of this format wrote, and carrying it forward
        // would mean writing a root id the root table has no entry for.
        let mut remap = vec![u32::MAX; v.len()];
        for id in 0..v.len() as u32 {
            if v.root[id as usize] != WORKSPACE {
                continue;
            }
            remap[id as usize] = out.nodes.len() as u32;
            out.nodes.push(v.node(id));
        }
        for (i, n) in out.nodes.iter().enumerate() {
            out.ids.insert(n.clone(), i as u32);
        }
        out.fwd = vec![Vec::new(); out.nodes.len()];
        for to in 0..v.len() as u32 {
            if remap[to as usize] == u32::MAX {
                continue;
            }
            for &from in v.rdeps.row(to) {
                if remap[from as usize] != u32::MAX {
                    out.fwd[remap[from as usize] as usize].push(remap[to as usize]);
                }
            }
        }
        Some(out)
    }

    /// The root id a dump key's root names.
    ///
    /// Only the workspace gets one. Everything else answers `NOT_MINE`, which
    /// is not a root id and never reaches a node: see [`SkyLayer::normalise`].
    /// The root column and the root table stay in the format because a rooted
    /// key is `[root]/[rel]` and reading one has to say which root it was, not
    /// because there is more than one to be.
    fn root_id(&mut self, root: &str, paths: &Paths) -> u32 {
        if paths.is_workspace(root) {
            return WORKSPACE;
        }
        // Kept so a layer that turned out to hold no file keys at all can say
        // where the dump's keys actually were rooted, which is the whole of
        // the diagnosis when a path does not match.
        self.foreign_roots
            .insert(root.trim_end_matches('/').to_string());
        NOT_MINE
    }

    /// The node a dump key names, or `None` for a key the layer drops.
    ///
    /// Dropped are the kinds that carry no loading-phase dependency a changed
    /// path can travel, and the keys that belong to *another repository*: a
    /// file under an external root, and the `PACKAGE`, `PACKAGE_LOOKUP`,
    /// `GLOB` and ignore nodes of a repository that is not the main one.
    ///
    /// Those are all sinks for this walk. Nothing in the loading phase depends
    /// on another repository's `PACKAGE` node, so a reverse walk that reached
    /// one would stop there, and the engine acts on a moved external
    /// repository through the externals stage rather than by re-querying a
    /// package it cannot query. Keeping them cost two things and bought
    /// nothing: the payload carried this machine's output base, and the layer
    /// *accumulated*, because a server along a walk loads repositories a cold
    /// build at the pin does not, so the incremental layer and the cold one
    /// stopped being comparable. The rebuild oracle is what found that, with
    /// four stale `@@bazel_ci_rules+...` packages after five steps.
    ///
    /// `BZL_LOAD` is kept whatever repository it names, and that is not an
    /// inconsistency. An external `.bzl` can be a main-repository *file*: a
    /// `local_path_override` on a module whose sources are in this tree (which
    /// is how tensorflow declares `third_party/xla`) gives a
    /// `BZL_LOAD:KeyForBuild{label=@@xla//:x.bzl}` hanging off
    /// `FILE:[<workspace>]/[third_party/xla/x.bzl]`, with main-repository
    /// packages above it. Dropping it would cut a chain that starts and ends
    /// in this repository.
    fn normalise(&mut self, key: &str, paths: &Paths) -> Option<SkyNode> {
        let (kind_str, rest) = dump::kind_of(key)?;
        let kind = Kind::from_dump(kind_str)?;
        if kind.is_rooted() {
            let (root, rel) = dump::split_rooted(rest)?;
            let root = self.root_id(root, paths);
            if root != WORKSPACE {
                return None;
            }
            return Some(SkyNode {
                kind,
                root,
                text: rel.to_string(),
            });
        }
        let text = erase(rest, paths);
        let mine = match kind {
            Kind::Package | Kind::PackageLookup => is_main_package(&text),
            // `IGNORED_SUBDIRECTORIES:@@` is the main repository's ignore
            // list; `IGNORED_PACKAGE_PREFIXES:@@rules_java_builtin` is
            // another repository's.
            Kind::Ignored => text == "@@",
            // A glob key embeds its package root, which the workspace
            // placeholder marks.
            Kind::Glob => text.contains(WORKSPACE_MARK),
            _ => true,
        };
        mine.then_some(SkyNode {
            kind,
            root: WORKSPACE,
            text,
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
    /// This is a cold build's apply: the dump is the whole loading phase of a
    /// server that has just loaded the whole repository, so every row in it is
    /// the truth about that commit. [`SkyLayer::apply_refresh`] is the
    /// incremental one, and it is more careful.
    ///
    /// Returns how many blocks the dump held, kept or not.
    pub fn apply_dump(&mut self, text: &str, paths: &Paths) -> usize {
        self.apply(text, paths, None)
    }

    /// Adds every block of a *refresh* dump: the rows of the packages this run
    /// re-queried are replaced, and every other row is merged into.
    ///
    /// The distinction is the difference between a layer that drifts and one
    /// that does not. A refresh dump comes from a server that has loaded some
    /// of the repository, and a node can appear there with fewer dependencies
    /// than it has at that commit. Replacing wholesale then *loses* edges: a
    /// 40-step walk on the `bazel` subject ended with two packages missing the
    /// edge to their own glob node, which is an add under those packages that
    /// would no longer re-query them. Merging cannot lose an edge; the worst
    /// it keeps is one that has gone, which over-approximates and is what the
    /// rebuild oracle counts as extra rather than failing on.
    ///
    /// The exception is the packages the run asked Bazel to load, by name, and
    /// their glob nodes. There the dump is authoritative: a glob that no
    /// longer reaches a directory has to lose that edge, or a later commit
    /// would re-query a package for a file it no longer globs.
    pub fn apply_refresh(
        &mut self,
        text: &str,
        paths: &Paths,
        requeried: &BTreeSet<String>,
    ) -> usize {
        self.apply(text, paths, Some(requeried))
    }

    /// The packages of `requeried` a refresh dump did not speak for.
    ///
    /// A refresh replaces a re-queried package's row and merges into every
    /// other, so a dump that omits a package's block, or names it with fewer
    /// dependencies than it has, leaves the old row standing and says nothing.
    /// That is the same loss a cold dump can suffer (see
    /// [`SkyLayer::looks_loaded`]) reaching the incremental path: the splice
    /// query is streamed on the same kind of server, and the packages it
    /// re-queried can come back without their values.
    ///
    /// Read *before* the refresh is applied, against the layer as it stands,
    /// so "thinner than it was" is a fact and not a guess. A package the run
    /// deleted has no block to find and belongs in `gone`, not here.
    ///
    /// The three deps that matter are the ones a changed path arrives through:
    /// the package's own `PACKAGE_LOOKUP`, its glob node, and any `BZL_LOAD`.
    /// A package that had one and comes back without it has lost the edge,
    /// not the edge's reason: Bazel re-queried it and told us what it depends
    /// on, and it cannot depend on nothing.
    pub fn refresh_shortfall(
        &mut self,
        text: &str,
        paths: &Paths,
        requeried: &BTreeSet<String>,
    ) -> Vec<String> {
        // Only the package blocks, and only of packages this run re-queried:
        // a refresh dump of a whole repository is tens of megabytes and none
        // of the rest of it is being judged here.
        let mut blocks: Vec<(String, Vec<String>)> = Vec::new();
        dump::parse_dump(text, &mut |k, d| {
            if let Some((kind, rest)) = dump::kind_of(k) {
                if kind == "PACKAGE" && requeried.contains(rest) {
                    blocks.push((rest.to_string(), d.iter().map(|s| s.to_string()).collect()));
                }
            }
        });
        let mut named: BTreeSet<String> = BTreeSet::new();
        let mut thin: BTreeSet<String> = BTreeSet::new();
        for (pkg, deps) in blocks {
            named.insert(pkg.clone());
            let mut healthy = false;
            for dep in &deps {
                let Some(node) = self.normalise(dep, paths) else {
                    continue;
                };
                healthy |= match node.kind {
                    Kind::PackageLookup => node.text == pkg,
                    Kind::Glob => glob_names_package(&node.text, &pkg),
                    Kind::BzlLoad | Kind::File => true,
                    _ => false,
                };
                if healthy {
                    break;
                }
            }
            if !healthy {
                thin.insert(pkg);
            }
        }
        requeried
            .iter()
            .filter(|p| {
                // `//external` is judged by nobody: it is synthesised from the
                // workspace file, depends on nothing the layer keeps, and
                // nothing seeds into it. A WORKSPACE edit re-queries it, and
                // on sourcegraph that was two packages of which one could
                // never pass, which failed the run.
                if is_synthetic_package(p) {
                    return false;
                }
                // Only a package the layer already knows can be *missing*: a
                // package this run is the first to see has no old row to
                // leave standing, and the walk names it for itself.
                (!named.contains(*p) && self.has_package(p)) || thin.contains(*p)
            })
            .cloned()
            .collect()
    }

    /// Whether the layer holds a `PACKAGE` node for `pkg`.
    pub fn has_package(&self, pkg: &str) -> bool {
        self.nodes
            .iter()
            .any(|n| n.kind == Kind::Package && n.text == pkg)
    }

    /// `requeried` is `None` for a cold build (replace every row), or the
    /// packages a refresh may speak for.
    fn apply(&mut self, text: &str, paths: &Paths, requeried: Option<&BTreeSet<String>>) -> usize {
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
            let replace = match requeried {
                None => true,
                Some(pkgs) => authoritative(&node, pkgs),
            };
            let id = self.intern(node);
            let mut row: Vec<u32> = Vec::with_capacity(deps.len());
            for dep in deps {
                let Some(d) = self.normalise(&dep, paths) else {
                    continue;
                };
                row.push(self.intern(d));
            }
            if !replace {
                row.extend_from_slice(&self.fwd[id as usize]);
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
        let nodes = self.nodes;

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
        // One root, the workspace, written as the empty string. The table and
        // the per-node column stay because a rooted key is `[root]/[rel]` and
        // reading one back has to say which root it was.
        t.root_offsets.push(0);
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
    // erasing the longer path first keeps that case stable. Every spelling is
    // tried, because a key embeds whichever one Bazel resolved to.
    for b in &paths.output_bases {
        if out.contains(b.as_str()) {
            out = out.replace(b.as_str(), OUTPUT_BASE_MARK);
        }
    }
    for w in &paths.workspaces {
        if out.contains(w.as_str()) {
            out = out.replace(w.as_str(), WORKSPACE_MARK);
        }
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

    /// The name of the root a node sits under, or `<unknown root N>` for a
    /// root id the table has no entry for. Only an index an earlier build of
    /// this format wrote can carry one, and a reader that panicked on it would
    /// turn a stale artifact into a crash.
    pub fn root_name(&self, id: u32) -> std::borrow::Cow<'a, str> {
        let r = self.root[id as usize];
        if (r as usize) < self.roots.len() {
            std::borrow::Cow::Borrowed(self.roots.get(r))
        } else {
            std::borrow::Cow::Owned(format!("<unknown root {r}>"))
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
            let probe = (self.kind[mid], self.root[mid], self.keys.get(mid as u32));
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
        let mut start: Vec<u32> = Vec::with_capacity(seeds.len());
        for (kind, text) in seeds {
            match self.id_of(*kind, text) {
                Some(id) => start.push(id),
                // Normal, and counted rather than guessed at: a path Bazel's
                // loading phase never looked at has no node, and reaches
                // nothing through this layer.
                None => out.unknown_seeds += 1,
            }
        }
        let mut via_bzl: Vec<u32> = Vec::new();
        self.reach(&start, |id, kind, text| {
            out.visited += 1;
            if kind == Kind::BzlLoad {
                via_bzl.push(id);
            }
            if kind.is_package() {
                // A package of some *other* repository: `@@rules_cc+//pkg`.
                // Nothing in a main-repository diff re-queries it, and the
                // externals stage is what routes a change that moved one.
                if !is_main_package(text) {
                    out.external_packages += 1;
                } else if kind == Kind::Package {
                    out.packages.insert(text.to_string());
                } else {
                    out.lookups.insert(text.to_string());
                }
            }
        });

        // Which of those packages a changed Starlark file reached, as against
        // a changed `BUILD` file or a moved directory listing.
        //
        // The distinction is load-bearing rather than a statistic. A `.bzl`
        // edit that changes a rule's *implementation* leaves every rule proto
        // in the loading packages byte-identical, so re-querying the package
        // finds every rule hash unchanged; the only thing that catches it is
        // marking the package's rules direct outright. Knowing which packages
        // those are is what keeps that from meaning "every dirty package".
        //
        // A second walk rather than a bit carried through the first: the bit
        // is monotone but a node can be reached before the predecessor that
        // would have set it, and re-walking from the `BZL_LOAD` nodes already
        // found costs one more pass over a graph the first walk just paged in.
        self.reach(&via_bzl, |_, kind, text| {
            if kind == Kind::Package && is_main_package(text) {
                out.bzl_packages.insert(text.to_string());
            }
        });
        out
    }

    /// Whether the Starlark file at `path` is in the workspace load closure:
    /// `MODULE.bazel`, `WORKSPACE` or a module extension reaches it, so it can
    /// define repository rules whose output is not in the rule layer at all.
    ///
    /// Bazel says so in the `BZL_LOAD` key itself. A file loaded by a `BUILD`
    /// file has `KeyForBuild`; one the module system loads has `KeyForBzlmod`,
    /// `KeyForBzlmodBootstrap` (Bazel 8/9) or `KeyForWorkspace` (Bazel 7). A
    /// file loaded only by another workspace-level `.bzl` gets its own
    /// `KeyForBuild` node with a workspace-level one above it, so the question
    /// is transitive and the `BZL_LOAD -> BZL_LOAD` edges answer it.
    ///
    /// This replaces the witness that used to re-scan the head checkout when
    /// the statically parsed load closure had no node for a changed file. The
    /// closure could be short — an unresolvable load label, a package
    /// tombstoned as partial — and had no way to know it was. These edges
    /// exist because Bazel followed them.
    pub fn bzl_is_workspace_level(&self, path: &str) -> bool {
        let Some(file) = self.id_of(Kind::File, path) else {
            return false;
        };
        let mut seen = vec![false; self.len()];
        let mut stack = vec![file];
        seen[file as usize] = true;
        while let Some(id) = stack.pop() {
            for &up in self.rdeps.row(id) {
                if seen[up as usize] || self.kind_of(up) != Kind::BzlLoad {
                    continue;
                }
                if is_workspace_bzl_key(self.text(up)) {
                    return true;
                }
                seen[up as usize] = true;
                stack.push(up);
            }
        }
        false
    }

    /// Breadth-first over reverse edges from `start`, calling `visit` once per
    /// node reached, `start` included.
    fn reach(&self, start: &[u32], mut visit: impl FnMut(u32, Kind, &'a str)) {
        let mut seen = vec![false; self.len()];
        let mut queue: VecDeque<u32> = VecDeque::new();
        for &id in start {
            if !seen[id as usize] {
                seen[id as usize] = true;
                queue.push_back(id);
            }
        }
        while let Some(id) = queue.pop_front() {
            visit(id, self.kind_of(id), self.text(id));
            for &up in self.rdeps.row(id) {
                if !seen[up as usize] {
                    seen[up as usize] = true;
                    queue.push_back(up);
                }
            }
        }
    }
}

/// Whether a `PACKAGE`/`PACKAGE_LOOKUP` key names a package of the main
/// repository. External ones carry their repository: `@@rules_java//java`.
fn is_main_package(text: &str) -> bool {
    !text.starts_with('@') && !text.contains("//")
}

/// Whether this package is one Bazel synthesises rather than one a `BUILD`
/// file declares.
///
/// `//external` in the WORKSPACE world is the only one. Bazel builds it from
/// the workspace file alone, and its `PACKAGE` node's whole dependency list is
/// `PRECOMPUTED:starlark_semantics` and `EXTERNAL_PACKAGE:...` -- two kinds the
/// layer drops, because everything depends on them. So it has no lookup, no
/// glob, no load and no file of its own, nothing in the layer can seed into
/// it, and it can never satisfy a health test written for real packages.
/// Checked against a real 7.4.1 dump: of 644 packages it is the only one with
/// none of those dependencies, and it does not exist under bzlmod at all.
///
/// It is not exempt from being *refreshed* -- a WORKSPACE edit re-queries it
/// and its rows are replaced like anyone's -- only from being *judged*.
fn is_synthetic_package(text: &str) -> bool {
    text == "external"
}

/// Whether a dependency of a `PACKAGE` node is one a changed path travels.
///
/// The three the walk arrives through, plus the package's own `BUILD` file.
/// Every real package in a real dump has all four: the thinnest of apollo's
/// 644 has eleven dependencies and they include a `PACKAGE_LOOKUP`, a
/// `BZL_LOAD` and a `FILE`.
fn is_loading_dep(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::BzlLoad | Kind::Glob | Kind::PackageLookup | Kind::File
    )
}

/// Whether a refresh may replace this node's row outright, rather than merging
/// into it: the run asked Bazel for this package by name, so what came back is
/// the truth about it.
fn authoritative(node: &SkyNode, requeried: &BTreeSet<String>) -> bool {
    match node.kind {
        Kind::Package => requeried.contains(&node.text),
        // A glob key names its package in its own text, in either major's
        // spelling.
        Kind::Glob => requeried.iter().any(|p| glob_names_package(&node.text, p)),
        _ => false,
    }
}

/// Whether a glob key is `package`'s. Bazel 8/9 write
/// `packageIdentifier = pkg,`; Bazel 7 writes `packageName=pkg `.
fn glob_names_package(text: &str, package: &str) -> bool {
    for prefix in ["packageIdentifier = ", "packageName="] {
        let Some(i) = text.find(prefix) else { continue };
        let rest = &text[i + prefix.len()..];
        if let Some(tail) = rest.strip_prefix(package) {
            // The next character must not be one a package name can hold, or
            // `a` would claim `ab`'s glob.
            if !tail
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || "/._+-".contains(c))
            {
                return true;
            }
        }
    }
    false
}

/// Whether a `BZL_LOAD` key is one the module system asked for, rather than a
/// `BUILD` file. Verified spellings: `KeyForBzlmod` and
/// `KeyForBzlmodBootstrap` on Bazel 8/9, `KeyForWorkspace` on Bazel 7.
fn is_workspace_bzl_key(text: &str) -> bool {
    text.starts_with("KeyForBzlmod") || text.starts_with("KeyForWorkspace")
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
    /// Of `packages`, the ones a changed Starlark file reached through
    /// `BZL_LOAD` edges. Their rules are marked direct outright, because a
    /// rule implementation can change without any rule proto moving.
    pub bzl_packages: BTreeSet<String>,
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
pub fn seeds<'a>(changed: impl IntoIterator<Item = (&'a str, bool)>) -> BTreeSet<(Kind, String)> {
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
