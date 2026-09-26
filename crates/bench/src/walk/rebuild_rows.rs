//! The rebuild oracle's loading-layer rows, every one of them, classified
//! and kept beside the walk result.
//!
//! `walk-<pin>.json` records the rebuild oracle as a verdict and a line of
//! counts, and the line said "loading layer rows missing: 319" for a whole
//! box run without ever naming one. The rows are what an investigation
//! starts from, and a cap in the result JSON is a schema commitment, so they
//! go in a sidecar instead:
//!
//! ```text
//! <results>/<subject>/walk/rebuild-<pin>[-<ver>].json      at the pin
//! <results>/<subject>/walk/labels/<parent>..<head>/rebuild.json   mid-walk (--rebuild-every)
//! ```
//!
//! Each row is a node or an edge of the layer (`kind`, `root`, `key`), the
//! repository path it names when one can be read off the key, the package
//! (the one the key names, or the one whose directory holds the path at the
//! compared commit), and a [`RowClass`]. The classes are decided from three
//! things the walk has to hand: the subject's tree at the classification
//! base and at the compared commit (`git ls-tree`), the applied layer
//! itself, and the two indexes' `error_packages`.
//!
//! # What the cold side is
//!
//! The cold index is built on a fresh Bazel server: the walk stops the
//! checkout's server before it (`Runner::reference_build` in the walk
//! driver), so its layer holds what a `//...:*` load at the compared commit
//! evaluates and nothing else. It used to share the server that answered
//! every walk step and every `rdeps` oracle query, and a dump prints every
//! done node a server holds, so the reference carried rows the walk had left
//! there and this module read them as the applied layer's shortfall. On
//! envoy at `7ba51266` that was every one of the 306 unexplained rows and
//! every one of the 13449 rows filed as boundary probes. Most of the 306
//! name paths absent at the pin (patches and build files deleted inside the
//! window, `WORKSPACE` probes under directories absent at the pin, probes of
//! packages deleted inside the window); 8 name paths live at the pin that
//! only a dependency traversal reaches (`BUILD.protobuf-2.28.0.bazel` under
//! `bazel/external/cargo/remote`, the `api` directory `envoy_api`'s local
//! override watches, and `api/buf.yaml`). On xla at `076fe93f` the 314 are
//! expected to be the same kind: `patch_file`, `build_file` and
//! requirements-lock inputs that fetches on the walk's server wrote, and two
//! packages only a traversal reaches. That was read from the rows, not
//! reproduced on a fresh server, and the 4 rows for xla's root `WORKSPACE`
//! (a file that exists at the pin and that nothing in Starlark names) are
//! still unaccounted for; they may be the same root probes the variant's
//! doc describes for envoy.
//!
//! [`RowClass::BoundaryProbe`] was written for these envoy rows (in
//! `2868471`, after run A's 16468 and run C's 13755), on the premise that a
//! cold `//...` load stats `WORKSPACE` in every package directory. That
//! premise holds only under `--enable_workspace` and was wrong for envoy at
//! its pin (see the variant), so the class excused leftovers there. A leftover on the cold side is a
//! fault in the reference, not a row to excuse, and no class should be
//! written for one.
//!
//! # Why a row can legitimately be missing
//!
//! A refresh dumps only what re-querying the dirty packages made Bazel
//! evaluate, where a cold `//...` build walks every directory. So a
//! directory created inside the window that no re-queried package's glob
//! descended into has its listing, lookup and `BUILD` probe rows in the cold
//! layer and not in the applied one. Those rows are inert: every seed path
//! into the directory starts under it, and a `BUILD` landing there later is
//! named from the diff. That is [`RowClass::CreatedInWindow`], and it is
//! claimed only when the applied layer has no row at all under the created
//! directory: a refresh that covered it would have written at least its
//! listing, so a hole under a directory the applied layer does know is a
//! shortfall, not a `//...`-only row. The created root is always a
//! directory: a file added to a directory that already existed is a row the
//! refresh of its package had to write, and the class never claims it. Nor
//! does it claim a created directory that holds a `BUILD` or `BUILD.bazel`
//! at the compared commit: that directory is a package created inside the
//! window, which the engine had to name from the diff and query, so its
//! rows are owed rather than inert (a queried new package leaves at least
//! its lookup and `BUILD` rows under the root, so refusing the class there
//! costs a correct engine nothing).
//!
//! A package the cold build at the compared commit failed to load under
//! `--keep_going` has whatever rows its partial evaluation left, and the
//! applied layer, which loaded it, has others: [`RowClass::LoadError`]. Only
//! the rebuilt index's list can excuse a missing row; the applied index's
//! list is the lineage's history (the engine carries it through every
//! splice and never clears it), so a package that failed once early in the
//! window and loaded fine since is in it, and excusing its holes on that
//! account would pass a real shortfall. Everything else missing is
//! [`RowClass::Unexplained`], and that count is what the oracle's verdict is
//! built from.
//!
//! Extras never fail the oracle. A refresh cannot remove a node, so a file,
//! package or `.bzl` that stopped existing leaves its row behind:
//! [`RowClass::DeletedInWindow`] when the path is gone at the compared
//! commit, [`RowClass::LoadError`] when either index names its package as
//! failed, [`RowClass::Other`] when neither (a `load()` that was dropped, a
//! glob that stopped matching).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use cambium_index::sky::{Kind, SkyRef};
use cambium_index::IndexRef;
use serde::{Deserialize, Serialize};

use crate::oracles::rebuild::{SkyDiff, SkyItem, SkyName};

/// How many rows each list keeps. The counts stay exact past it and
/// `truncated` says so; a layer that is this far off is not read row by row.
pub const MAX_ROWS: usize = 100_000;

/// The mid-walk sidecar's file name, under the step's labels directory.
pub const MID_WALK_FILE: &str = "rebuild.json";

/// The sidecar document.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RebuildRows {
    pub schema: u32,
    pub repo: String,
    pub pin: String,
    /// The classification base: the head of the last cold rebuild inside the
    /// window (a fresh lineage resets the layer), else the window start.
    pub from: String,
    /// The compared commit: the pin, or a mid-walk head.
    pub at: String,
    pub bazel_version: String,
    pub counts: RowCounts,
    /// Rows the cold layer has and the applied one does not, unexplained
    /// first, then by class and name.
    pub missing: Vec<Row>,
    /// Rows the applied layer has and the cold one does not.
    pub extra: Vec<Row>,
    /// A list ran past [`MAX_ROWS`] and was cut; the counts are still exact.
    pub truncated: bool,
}

/// The tallies, exact whatever the lists hold.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RowCounts {
    pub missing: usize,
    /// `created_in_window`, `load_error` and `boundary_probe`.
    pub missing_expected: usize,
    /// What the oracle's verdict counts.
    pub missing_unexplained: usize,
    pub extra: usize,
    /// `deleted_in_window` plus `load_error`.
    pub extra_expected: usize,
}

/// One classified row.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Row {
    #[serde(flatten)]
    pub item: SkyItem,
    /// The repository path the key names, when it names one. A directory for
    /// a package or a listing, a file otherwise.
    #[serde(default)]
    pub path: Option<String>,
    /// The package the key names or, for a file, listing or `.bzl` row, the
    /// package whose directory holds the path at the compared commit.
    /// Absent when neither applies (another repository's row, a driver with
    /// no tree, a path under no package).
    #[serde(default)]
    pub package: Option<String>,
    pub class: RowClass,
}

/// Why a row differs. Declared in the order the lists are sorted.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RowClass {
    /// A missing row nothing accounts for: the shortfall the oracle fails on.
    Unexplained,
    /// Missing, under a directory created inside the window that the applied
    /// layer holds no row under.
    CreatedInWindow,
    /// Missing: its package failed to load in the cold build at the
    /// compared commit. Extra: either index names its package as failed.
    LoadError,
    /// Missing: a `FILE` or `FILE_STATE` row for a workspace or build file
    /// that does not exist at the compared commit, in a directory that does.
    /// Under `--enable_workspace`, Bazel stats `WORKSPACE` and
    /// `WORKSPACE.bazel` in every directory whose package it looks up, to
    /// find local repository boundaries (with it off, Bazel 8.7.0's
    /// `LocalRepositoryLookupFunction` returns before any stat); in either
    /// mode it probes the `BUILD` file names in turn, so `BUILD.bazel` is
    /// statted beside every `BUILD`. `REPO.bazel` and `MODULE.bazel` are in
    /// the list as the other boundary markers. A cold dump carries those
    /// absent-file probes for every directory `//...` visits and a refresh
    /// dump only for the packages it re-queried. The file's later creation
    /// is a `BUILD` the classifier names from the diff, or a boundary
    /// marker, which is a default invalidator, so no change travels through
    /// the missing row.
    ///
    /// The class is right for a WORKSPACE-mode subject at its own pin. It is
    /// not what envoy's 13449 were: envoy's pin runs `--noenable_workspace`,
    /// so a `//...:*` load there writes no per-directory `WORKSPACE` probe.
    /// Those rows (every one a `WORKSPACE`, `WORKSPACE.bazel` or
    /// `WORKSPACE.bzlmod` row) were left on the server the reference build
    /// used to share. The per-directory ones came from the walk's
    /// WORKSPACE-era steps. The root ones came from some other command on
    /// that server, and not from WORKSPACE mode: a 3-step window
    /// `b6e7e12..7ba5126`, entirely after envoy's switch to
    /// `--noenable_workspace` and started on a fresh server, still put the
    /// root `WORKSPACE`, `WORKSPACE.bazel` and `WORKSPACE.bzlmod` rows (12
    /// with their edges) in a shared-server reference, and none in a fresh
    /// one. Which command writes them is not established. See "What the cold
    /// side is" above.
    BoundaryProbe,
    /// Extra, for a path gone at the compared commit.
    DeletedInWindow,
    /// Extra, for a path that still exists: a dropped `load()`, a glob that
    /// stopped matching. Inert either way.
    Other,
}

impl RebuildRows {
    /// `<walk_dir>/rebuild-<pin>[-<ver>].json`.
    pub fn path(walk_dir: &Path, pin: &str, version: Option<&str>) -> PathBuf {
        walk_dir.join(crate::util::versioned_name(
            &format!("rebuild-{pin}"),
            version,
            "json",
        ))
    }

    /// Sorts, tallies and caps the classified rows into a document.
    pub fn new(
        repo: &str,
        pin: &str,
        from: &str,
        at: &str,
        bazel_version: &str,
        mut missing: Vec<Row>,
        mut extra: Vec<Row>,
    ) -> Self {
        let by_class_then_name =
            |a: &Row, b: &Row| a.class.cmp(&b.class).then_with(|| a.item.cmp(&b.item));
        missing.sort_by(by_class_then_name);
        extra.sort_by(by_class_then_name);
        let counts = counts(&missing, &extra);
        let truncated = missing.len() > MAX_ROWS || extra.len() > MAX_ROWS;
        missing.truncate(MAX_ROWS);
        extra.truncate(MAX_ROWS);
        RebuildRows {
            schema: crate::RESULT_SCHEMA,
            repo: repo.to_string(),
            pin: pin.to_string(),
            from: from.to_string(),
            at: at.to_string(),
            bazel_version: bazel_version.to_string(),
            counts,
            missing,
            extra,
            truncated,
        }
    }

    pub fn read(path: &Path) -> Result<Self> {
        crate::util::read_result(path)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        crate::util::write_json_atomic(path, self)
    }
}

/// The tallies over two classified lists.
pub fn counts(missing: &[Row], extra: &[Row]) -> RowCounts {
    let of = |rows: &[Row], class: RowClass| rows.iter().filter(|r| r.class == class).count();
    RowCounts {
        missing: missing.len(),
        missing_expected: of(missing, RowClass::CreatedInWindow)
            + of(missing, RowClass::LoadError)
            + of(missing, RowClass::BoundaryProbe),
        missing_unexplained: of(missing, RowClass::Unexplained),
        extra: extra.len(),
        extra_expected: of(extra, RowClass::DeletedInWindow) + of(extra, RowClass::LoadError),
    }
}

/// The repository path and the package a node's key names, read off the key.
///
/// A rooted kind under the workspace names its relative path; a package
/// kind names its directory and itself, unless it is another repository's
/// (`@`); a glob names its package; a `BZL_LOAD` of a main-repository label
/// names the `.bzl` file. The ignore singleton, a tombstone and anything
/// under another root name nothing.
pub fn locate(name: &SkyName) -> (Option<String>, Option<String>) {
    match name.kind {
        Kind::File | Kind::FileState | Kind::DirectoryListing | Kind::DirectoryListingState => {
            if name.root.is_empty() {
                (Some(name.key.clone()), None)
            } else {
                (None, None)
            }
        }
        Kind::Package | Kind::PackageLookup => {
            if name.key.starts_with('@') {
                (None, None)
            } else {
                (Some(name.key.clone()), Some(name.key.clone()))
            }
        }
        Kind::Glob => match cambium_index::sky::glob_package(&name.key) {
            Some(p) if !p.starts_with('@') => (Some(p.to_string()), Some(p.to_string())),
            _ => (None, None),
        },
        Kind::BzlLoad => (bzl_path(&name.key), None),
        Kind::Ignored | Kind::Tombstone => (None, None),
    }
}

/// `pkg/file.bzl` from `KeyForBuild{label=@@//pkg:file.bzl, ...}`; `None`
/// for a label of another repository.
fn bzl_path(key: &str) -> Option<String> {
    let i = key.find("label=")?;
    let rest = &key[i + "label=".len()..];
    let end = rest.find([',', '}', ' ']).unwrap_or(rest.len());
    let label = rest[..end].trim_start_matches('@');
    let rel = label.strip_prefix("//")?;
    let (pkg, file) = rel.split_once(':')?;
    Some(if pkg.is_empty() {
        file.to_string()
    } else {
        format!("{pkg}/{file}")
    })
}

/// Whether `p` is a file in the tree or a directory some file sits under.
/// The empty path is the root package and always exists.
pub fn exists(tree: &BTreeSet<String>, p: &str) -> bool {
    tree.contains(p) || is_dir(tree, p)
}

/// Whether `p` is a directory in the tree: some file sits under it. The
/// empty path is the root and always is.
pub fn is_dir(tree: &BTreeSet<String>, p: &str) -> bool {
    if p.is_empty() {
        return true;
    }
    let dir = format!("{p}/");
    tree.range(dir.clone()..)
        .next()
        .is_some_and(|f| f.starts_with(&dir))
}

/// The package whose directory holds `p` in the tree: the nearest of `p`
/// (when it is a directory) and its ancestors with a `BUILD` or
/// `BUILD.bazel`, the root package included. `None` when none has one.
pub fn enclosing_package(tree: &BTreeSet<String>, p: &str) -> Option<String> {
    let mut dir = p;
    loop {
        if ["BUILD", "BUILD.bazel"]
            .iter()
            .any(|b| tree.contains(&join(dir, b)))
        {
            return Some(dir.to_string());
        }
        if dir.is_empty() {
            return None;
        }
        dir = dir.rsplit_once('/').map_or("", |(d, _)| d);
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// The paths the applied layer's nodes name, for the created-in-window test.
pub fn applied_paths(layer: SkyRef<'_>) -> BTreeSet<String> {
    (0..layer.len() as u32)
        .filter_map(|id| {
            let name = SkyName {
                kind: layer.kind_of(id),
                root: layer.root_name(id).into_owned(),
                key: layer.text(id).to_string(),
            };
            locate(&name).0
        })
        .collect()
}

/// The packages each index says failed to load under `--keep_going`.
///
/// The rebuilt index's list is the truth about the compared commit: a cold
/// build names exactly the packages it could not load there. The applied
/// index's list is the lineage's history, not its state: the engine carries
/// it through every splice, assigns it only when it is empty, and caps it,
/// so it holds the first partial refresh's names and keeps them past the
/// commit that fixed them. A missing row (the cold layer has it, the applied
/// lacks it) can only be a load-error artifact when the cold build itself
/// failed the package, so the rebuilt list alone excuses one. An extra never
/// fails, and either list may explain it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ErrorPackages {
    pub rebuilt: BTreeSet<String>,
    pub applied: BTreeSet<String>,
}

impl ErrorPackages {
    pub fn of(applied: IndexRef<'_>, rebuilt: IndexRef<'_>) -> Self {
        ErrorPackages {
            rebuilt: rebuilt.error_packages.iter().cloned().collect(),
            applied: applied.error_packages.iter().cloned().collect(),
        }
    }

    /// Whether a missing row of `package` is a load-error artifact.
    fn excuses_missing(&self, package: &str) -> bool {
        self.rebuilt.contains(package)
    }

    /// Whether an extra row of `package` is.
    fn excuses_extra(&self, package: &str) -> bool {
        self.rebuilt.contains(package) || self.applied.contains(package)
    }
}

/// Classifies every row of a diff.
///
/// `from` and `to` are the trees at the classification base and at the
/// compared commit; with either absent nothing is expected, every missing
/// row is unexplained and every extra is `other`. `applied_paths` is
/// [`applied_paths`] over the applied layer, `errors` both indexes'
/// `error_packages` ([`ErrorPackages`]).
pub fn classify(
    diff: &SkyDiff,
    from: Option<&BTreeSet<String>>,
    to: Option<&BTreeSet<String>>,
    applied_paths: &BTreeSet<String>,
    errors: &ErrorPackages,
) -> (Vec<Row>, Vec<Row>) {
    let trees = from.zip(to);
    let missing = diff
        .missing
        .iter()
        .map(|item| row(item, missing_class(item, trees, applied_paths, errors), to))
        .collect();
    let extra = diff
        .extra
        .iter()
        .map(|item| row(item, extra_class(item, to, errors), to))
        .collect();
    (missing, extra)
}

fn row(item: &SkyItem, class: RowClass, to: Option<&BTreeSet<String>>) -> Row {
    let (path, package) = located(item, to);
    Row {
        item: item.clone(),
        path,
        package,
        class,
    }
}

/// [`locate`], with the package a file, listing or `.bzl` row sits in read
/// off the tree at the compared commit when the key itself names none. A
/// package that failed to load has its glob inputs and probes on one side
/// and not the other, and those rows name the package only through the
/// tree.
fn locate_in(name: &SkyName, to: Option<&BTreeSet<String>>) -> (Option<String>, Option<String>) {
    let (path, package) = locate(name);
    let package = package.or_else(|| {
        let (p, tree) = path.as_deref().zip(to)?;
        enclosing_package(tree, p)
    });
    (path, package)
}

/// An edge is located by its `to` end (the dependency that is absent),
/// falling back to `from`.
fn located(item: &SkyItem, to: Option<&BTreeSet<String>>) -> (Option<String>, Option<String>) {
    match item {
        SkyItem::Node(n) => locate_in(n, to),
        SkyItem::Edge { from, to: dep } => match locate_in(dep, to) {
            (None, None) => locate_in(from, to),
            l => l,
        },
    }
}

fn endpoints(item: &SkyItem) -> Vec<&SkyName> {
    match item {
        SkyItem::Node(n) => vec![n],
        SkyItem::Edge { from, to } => vec![to, from],
    }
}

type Trees<'a> = Option<(&'a BTreeSet<String>, &'a BTreeSet<String>)>;

/// A missing edge is created-in-window if either end is, a load error if
/// either end's package failed, else unexplained.
fn missing_class(
    item: &SkyItem,
    trees: Trees<'_>,
    applied_paths: &BTreeSet<String>,
    errors: &ErrorPackages,
) -> RowClass {
    endpoints(item)
        .into_iter()
        .map(|n| missing_node_class(n, trees, applied_paths, errors))
        .max_by_key(|c| match c {
            RowClass::CreatedInWindow => 3,
            RowClass::LoadError => 2,
            RowClass::BoundaryProbe => 1,
            _ => 0,
        })
        .unwrap_or(RowClass::Unexplained)
}

fn missing_node_class(
    name: &SkyName,
    trees: Trees<'_>,
    applied_paths: &BTreeSet<String>,
    errors: &ErrorPackages,
) -> RowClass {
    let Some((from, to)) = trees else {
        return RowClass::Unexplained;
    };
    let (path, package) = locate_in(name, Some(to));
    if let Some(root) = path.as_deref().and_then(|p| created_root(p, from, to)) {
        if !exists(applied_paths, &root) && !holds_build_file(to, &root) {
            return RowClass::CreatedInWindow;
        }
    }
    if package.is_some_and(|p| errors.excuses_missing(&p)) {
        return RowClass::LoadError;
    }
    // The probe is only a probe in a directory that is there to be probed;
    // a row for a file under a directory that exists nowhere is still
    // unexplained.
    if matches!(name.kind, Kind::File | Kind::FileState)
        && path.as_deref().is_some_and(|p| {
            let dir = p.rsplit_once('/').map_or("", |(d, _)| d);
            is_boundary_probe(p) && !exists(to, p) && (dir.is_empty() || is_dir(to, dir))
        })
    {
        return RowClass::BoundaryProbe;
    }
    RowClass::Unexplained
}

/// Whether a path is one Bazel stats in a directory without anyone naming
/// it: a repository boundary marker, or the other spelling of a `BUILD` file.
fn is_boundary_probe(p: &str) -> bool {
    matches!(
        p.rsplit_once('/').map_or(p, |(_, f)| f),
        "WORKSPACE"
            | "WORKSPACE.bazel"
            | "WORKSPACE.bzlmod"
            | "REPO.bazel"
            | "MODULE.bazel"
            | "BUILD"
            | "BUILD.bazel"
    )
}

/// The directory created inside the window that `p` sits under: the
/// shallowest prefix, absent at `from`, of the deepest ancestor of `p` that
/// is a directory at `to` (`p` itself when it is one; a `BUILD` probe of a
/// directory with no `BUILD` resolves through the directory). `None` when
/// that ancestor already existed at `from`, or when `p` has none.
///
/// A file is never the root. A file added to a directory that existed at
/// `from` has no created directory above it, and its own rows are exactly
/// what the refresh of its package had to write; a `.bzl` in particular has
/// consumers, and a layer without its `BZL_LOAD` node dirties nothing when
/// it changes.
fn created_root(p: &str, from: &BTreeSet<String>, to: &BTreeSet<String>) -> Option<String> {
    let mut anchor = p;
    while !exists(to, anchor) {
        anchor = anchor.rsplit_once('/').map_or("", |(dir, _)| dir);
    }
    if anchor.is_empty() {
        return None;
    }
    // Every proper prefix of the anchor is a directory at `to`; the anchor
    // itself is one only when something sits under it.
    let ancestors = anchor.match_indices('/').map(|(i, _)| &anchor[..i]);
    let whole = is_dir(to, anchor).then_some(anchor);
    ancestors
        .chain(whole)
        .find(|prefix| !exists(from, prefix))
        .map(str::to_string)
}

/// Whether the tree holds a `BUILD` or `BUILD.bazel` anywhere under `dir`.
///
/// A directory created inside the window that holds one is a package the
/// engine had to name from the diff and query, so a missing row under it is
/// a refresh shortfall and not a `//...`-only row.
fn holds_build_file(tree: &BTreeSet<String>, dir: &str) -> bool {
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    tree.range(prefix.clone()..)
        .take_while(|f| f.starts_with(&prefix))
        .any(|f| {
            let base = f.rsplit_once('/').map_or(f.as_str(), |(_, n)| n);
            base == "BUILD" || base == "BUILD.bazel"
        })
}

/// An extra is deleted-in-window when an end names a path gone at `to`, a
/// load error when either index says an end's package failed, else other.
/// With no tree, other.
fn extra_class(item: &SkyItem, to: Option<&BTreeSet<String>>, errors: &ErrorPackages) -> RowClass {
    let Some(to) = to else {
        return RowClass::Other;
    };
    let ends: Vec<(Option<String>, Option<String>)> = endpoints(item)
        .into_iter()
        .map(|n| locate_in(n, Some(to)))
        .collect();
    if ends
        .iter()
        .any(|(p, _)| p.as_deref().is_some_and(|p| !exists(to, p)))
    {
        return RowClass::DeletedInWindow;
    }
    if ends
        .iter()
        .any(|(_, pkg)| pkg.as_deref().is_some_and(|p| errors.excuses_extra(p)))
    {
        return RowClass::LoadError;
    }
    RowClass::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(kind: Kind, key: &str) -> SkyName {
        SkyName {
            kind,
            root: String::new(),
            key: key.to_string(),
        }
    }

    fn tree(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        tree(items)
    }

    /// Packages the cold build at the compared commit failed to load.
    fn errs(rebuilt: &[&str]) -> ErrorPackages {
        ErrorPackages {
            rebuilt: set(rebuilt),
            applied: BTreeSet::new(),
        }
    }

    fn diff(missing: Vec<SkyItem>, extra: Vec<SkyItem>) -> SkyDiff {
        SkyDiff {
            present: None,
            missing,
            extra,
        }
    }

    fn classes(rows: &[Row]) -> Vec<RowClass> {
        rows.iter().map(|r| r.class).collect()
    }

    const FROM: &[&str] = &["BUILD", "lib/BUILD", "lib/a.cc", "old/BUILD", "old/x.txt"];
    const TO: &[&str] = &[
        "BUILD",
        "lib/BUILD",
        "lib/a.cc",
        "newdir/sub/f.txt",
        "newpkg/BUILD",
        "newpkg/data.txt",
        "lib/old.bzl",
    ];

    /// [`classify`] over the window `FROM..TO`, with the applied paths and
    /// the packages that failed to load in the rebuild.
    fn in_window(d: &SkyDiff, applied: &[&str], failed: &[&str]) -> (Vec<Row>, Vec<Row>) {
        classify(
            d,
            Some(&tree(FROM)),
            Some(&tree(TO)),
            &set(applied),
            &errs(failed),
        )
    }

    /// (i) A node under a directory the window created, with nothing of the
    /// applied layer under it, is a `//...`-only row.
    #[test]
    fn a_node_under_a_new_directory_the_applied_layer_never_saw_is_expected() {
        let d = diff(
            vec![SkyItem::Node(name(Kind::PackageLookup, "newdir/sub"))],
            vec![],
        );
        let (m, _) = in_window(&d, &["lib"], &[]);
        assert_eq!(classes(&m), vec![RowClass::CreatedInWindow]);
        assert_eq!(m[0].path.as_deref(), Some("newdir/sub"));
        assert_eq!(m[0].package.as_deref(), Some("newdir/sub"));
    }

    /// (ii) The same row when the applied layer does hold something under
    /// the new directory: a refresh covered it, so the hole is real.
    #[test]
    fn a_hole_under_a_new_directory_the_applied_layer_knows_is_unexplained() {
        let d = diff(
            vec![SkyItem::Node(name(Kind::PackageLookup, "newdir/sub"))],
            vec![],
        );
        let (m, _) = in_window(&d, &["newdir/BUILD"], &[]);
        assert_eq!(classes(&m), vec![RowClass::Unexplained]);
    }

    /// (ii-b) A directory created in the window that holds a `BUILD` is a
    /// package the engine had to name from the diff and query, so its rows
    /// are owed however little the applied layer knows of it.
    #[test]
    fn a_hole_in_a_package_created_in_the_window_is_unexplained() {
        let d = diff(
            vec![
                SkyItem::Node(name(Kind::PackageLookup, "newpkg")),
                SkyItem::Node(name(Kind::Package, "newpkg")),
                SkyItem::Node(name(Kind::FileState, "newpkg/BUILD")),
                SkyItem::Node(name(Kind::File, "newpkg/data.txt")),
                SkyItem::Edge {
                    from: name(Kind::Package, "newpkg"),
                    to: name(Kind::File, "newpkg/data.txt"),
                },
            ],
            vec![],
        );
        let (m, _) = in_window(&d, &["lib"], &[]);
        assert_eq!(classes(&m), vec![RowClass::Unexplained; 5], "{m:?}");
    }

    /// (iii) A path that existed at the base had every chance to be written.
    #[test]
    fn a_probe_for_a_boundary_file_that_does_not_exist_is_expected() {
        // Bazel's stat of `lib/WORKSPACE.bazel` while deciding whether `lib`
        // is a repository, and of `lib/BUILD.bazel` beside `lib/BUILD`.
        // Neither exists at either end; a source file that does not exist
        // is still unexplained, and a probe for a file that *does* exist is
        // a real row.
        let d = diff(
            vec![
                SkyItem::Node(name(Kind::FileState, "lib/WORKSPACE.bazel")),
                SkyItem::Node(name(Kind::File, "lib/BUILD.bazel")),
                SkyItem::Node(name(Kind::File, "lib/gone.cc")),
                SkyItem::Node(name(Kind::File, "lib/BUILD")),
            ],
            vec![],
        );
        let (m, _) = in_window(&d, &[], &[]);
        assert_eq!(
            classes(&m),
            vec![
                RowClass::BoundaryProbe,
                RowClass::BoundaryProbe,
                RowClass::Unexplained,
                RowClass::Unexplained
            ]
        );
        let c = counts(&m, &[]);
        assert_eq!(
            (c.missing, c.missing_expected, c.missing_unexplained),
            (4, 2, 2)
        );
    }

    #[test]
    fn a_path_present_at_the_base_is_unexplained() {
        let d = diff(vec![SkyItem::Node(name(Kind::File, "lib/a.cc"))], vec![]);
        let (m, _) = in_window(&d, &[], &[]);
        assert_eq!(classes(&m), vec![RowClass::Unexplained]);
    }

    /// (iv) A package either side failed to load has different rows on each.
    #[test]
    fn a_package_that_failed_to_load_is_a_load_error() {
        let d = diff(
            vec![
                SkyItem::Node(name(Kind::Package, "lib")),
                SkyItem::Node(name(
                    Kind::Glob,
                    "<GlobsKey packageRoot = <workspace>, packageIdentifier = lib, globRequests = []>",
                )),
            ],
            vec![],
        );
        let (m, _) = in_window(&d, &[], &["lib"]);
        assert_eq!(classes(&m), vec![RowClass::LoadError, RowClass::LoadError]);
    }

    /// (v) A `BUILD` probe of a directory that has no `BUILD` at either
    /// commit resolves through the directory it probes.
    #[test]
    fn a_build_probe_of_a_new_directory_resolves_through_its_anchor() {
        let d = diff(
            vec![SkyItem::Node(name(Kind::FileState, "newdir/sub/BUILD"))],
            vec![],
        );
        let (m, _) = in_window(&d, &[], &[]);
        assert_eq!(classes(&m), vec![RowClass::CreatedInWindow]);
        // But a probe of a directory that exists nowhere resolves to nothing.
        let d = diff(
            vec![SkyItem::Node(name(Kind::FileState, "nowhere/BUILD"))],
            vec![],
        );
        let (m, _) = in_window(&d, &[], &[]);
        assert_eq!(classes(&m), vec![RowClass::Unexplained]);
    }

    /// (vi) An edge classifies as its ends do.
    #[test]
    fn an_edge_into_a_new_directory_is_expected_with_its_end() {
        let d = diff(
            vec![SkyItem::Edge {
                from: name(Kind::Package, "lib"),
                to: name(Kind::PackageLookup, "newdir"),
            }],
            vec![],
        );
        let (m, _) = in_window(&d, &["lib"], &[]);
        assert_eq!(classes(&m), vec![RowClass::CreatedInWindow]);
        assert_eq!(m[0].path.as_deref(), Some("newdir"));
        // A load error at either end is a load error.
        let d = diff(
            vec![SkyItem::Edge {
                from: name(Kind::Package, "lib"),
                to: name(Kind::File, "lib/a.cc"),
            }],
            vec![],
        );
        let (m, _) = in_window(&d, &[], &["lib"]);
        assert_eq!(classes(&m), vec![RowClass::LoadError]);
    }

    /// (vii) An extra for a path gone at the compared commit is what a
    /// refresh cannot remove; one for a surviving path is something else.
    #[test]
    fn an_extra_is_deleted_in_window_only_when_its_path_is_gone() {
        let d = diff(
            vec![],
            vec![
                SkyItem::Node(name(Kind::File, "old/x.txt")),
                SkyItem::Node(name(Kind::PackageLookup, "old")),
                SkyItem::Edge {
                    from: name(Kind::Package, "lib"),
                    to: name(Kind::File, "lib/old.bzl"),
                },
            ],
        );
        let (_, e) = in_window(&d, &[], &[]);
        assert_eq!(
            classes(&e),
            vec![
                RowClass::DeletedInWindow,
                RowClass::DeletedInWindow,
                RowClass::Other
            ]
        );
    }

    /// (viii) A driver with no tree expects nothing.
    #[test]
    fn with_no_trees_every_missing_row_is_unexplained() {
        let d = diff(
            vec![
                SkyItem::Node(name(Kind::PackageLookup, "newdir/sub")),
                SkyItem::Node(name(Kind::Package, "lib")),
            ],
            vec![SkyItem::Node(name(Kind::File, "old/x.txt"))],
        );
        let (m, e) = classify(&d, None, None, &set(&[]), &errs(&["lib"]));
        assert_eq!(
            classes(&m),
            vec![RowClass::Unexplained, RowClass::Unexplained]
        );
        assert_eq!(classes(&e), vec![RowClass::Other]);
        let (m, _) = classify(&d, Some(&tree(FROM)), None, &set(&[]), &errs(&[]));
        assert_eq!(
            classes(&m),
            vec![RowClass::Unexplained, RowClass::Unexplained]
        );
    }

    /// (ix) The applied index's list is history: a package that failed once
    /// in the window and loads at the pin cannot excuse a hole in its rows.
    /// An extra never fails, and either list explains one.
    #[test]
    fn only_the_cold_builds_load_errors_excuse_a_missing_row() {
        let d = diff(
            vec![
                SkyItem::Node(name(Kind::Package, "lib")),
                SkyItem::Edge {
                    from: name(Kind::Package, "lib"),
                    to: name(Kind::PackageLookup, "lib/sub"),
                },
            ],
            vec![SkyItem::Node(name(Kind::Package, "lib"))],
        );
        let applied_only = ErrorPackages {
            rebuilt: set(&[]),
            applied: set(&["lib"]),
        };
        let (m, e) = classify(
            &d,
            Some(&tree(FROM)),
            Some(&tree(TO)),
            &set(&[]),
            &applied_only,
        );
        assert_eq!(
            classes(&m),
            vec![RowClass::Unexplained, RowClass::Unexplained]
        );
        assert_eq!(classes(&e), vec![RowClass::LoadError]);
        let both = ErrorPackages {
            rebuilt: set(&["lib"]),
            applied: set(&["lib"]),
        };
        let (m, e) = classify(&d, Some(&tree(FROM)), Some(&tree(TO)), &set(&[]), &both);
        assert_eq!(classes(&m), vec![RowClass::LoadError, RowClass::LoadError]);
        assert_eq!(classes(&e), vec![RowClass::LoadError]);
    }

    /// (x) A failed package's glob inputs and probes (its files, listings,
    /// `.bzl` files) name the package only through the tree, and they are
    /// load errors with the rest of it. The row carries the package it was
    /// placed in.
    #[test]
    fn a_failed_packages_file_and_listing_rows_are_load_errors_too() {
        let d = diff(
            vec![
                SkyItem::Node(name(Kind::FileState, "lib/a.cc")),
                SkyItem::Node(name(Kind::DirectoryListing, "lib")),
                SkyItem::Node(name(Kind::DirectoryListingState, "lib")),
                SkyItem::Node(name(Kind::File, "lib/BUILD")),
                SkyItem::Node(name(
                    Kind::BzlLoad,
                    "KeyForBuild{label=@@//lib:old.bzl, isBuildPrelude=false}",
                )),
                SkyItem::Edge {
                    from: name(Kind::Glob, "<GlobsKey packageRoot = <workspace>, packageIdentifier = lib, globRequests = []>"),
                    to: name(Kind::FileState, "lib/a.cc"),
                },
            ],
            vec![SkyItem::Node(name(Kind::FileState, "lib/a.cc"))],
        );
        let (m, e) = in_window(&d, &[], &["lib"]);
        assert_eq!(classes(&m), vec![RowClass::LoadError; 6], "{m:?}");
        assert!(
            m.iter().all(|r| r.package.as_deref() == Some("lib")),
            "{m:?}"
        );
        assert_eq!(classes(&e), vec![RowClass::LoadError]);
        // A file under no package names none, and is not excused.
        let d = diff(
            vec![SkyItem::Node(name(Kind::FileState, "old/x.txt"))],
            vec![],
        );
        let to = tree(&["BUILD", "old/x.txt"]);
        let from = tree(&["BUILD", "old/x.txt"]);
        let no_root: BTreeSet<String> = ["old/x.txt".to_string()].into();
        let (m, _) = classify(
            &d,
            Some(&no_root),
            Some(&no_root),
            &set(&[]),
            &errs(&["old"]),
        );
        assert_eq!(classes(&m), vec![RowClass::Unexplained]);
        assert_eq!(m[0].package, None);
        // With a root `BUILD` the root package holds it.
        let (m, _) = classify(&d, Some(&from), Some(&to), &set(&[]), &errs(&[""]));
        assert_eq!(classes(&m), vec![RowClass::LoadError]);
        assert_eq!(m[0].package.as_deref(), Some(""));
    }

    /// (xi) A file added inside the window to a directory that existed at
    /// the base is not under any created directory: its rows are what the
    /// refresh of its package had to write, and a hole there is a hole.
    #[test]
    fn a_new_file_in_a_directory_that_existed_at_the_base_is_unexplained() {
        let from = tree(&["BUILD", "lib/BUILD", "lib/a.txt"]);
        let to = tree(&[
            "BUILD",
            "lib/BUILD",
            "lib/a.txt",
            "lib/b.txt",
            "lib/new.bzl",
        ]);
        let d = diff(
            vec![
                SkyItem::Node(name(Kind::File, "lib/b.txt")),
                SkyItem::Node(name(Kind::FileState, "lib/b.txt")),
                SkyItem::Edge {
                    from: name(Kind::Glob, "<GlobsKey packageRoot = <workspace>, packageIdentifier = lib, globRequests = []>"),
                    to: name(Kind::File, "lib/b.txt"),
                },
                SkyItem::Node(name(
                    Kind::BzlLoad,
                    "KeyForBuild{label=@@//lib:new.bzl, isBuildPrelude=false}",
                )),
                SkyItem::Edge {
                    from: name(Kind::Package, "lib"),
                    to: name(
                        Kind::BzlLoad,
                        "KeyForBuild{label=@@//lib:new.bzl, isBuildPrelude=false}",
                    ),
                },
            ],
            vec![],
        );
        // Whether or not the applied layer knows the directory.
        for applied in [set(&[]), set(&["lib", "lib/BUILD", "lib/a.txt"])] {
            let (m, _) = classify(&d, Some(&from), Some(&to), &applied, &errs(&[]));
            assert_eq!(classes(&m), vec![RowClass::Unexplained; 5], "{m:?}");
            assert!(
                m.iter().all(|r| r.package.as_deref() == Some("lib")),
                "{m:?}"
            );
        }
        // A new directory beside them is still a created root, and a new
        // file inside a new directory sits under that root.
        let to = tree(&["BUILD", "lib/BUILD", "lib/a.txt", "lib/gen/out.txt"]);
        let d = diff(
            vec![
                SkyItem::Node(name(Kind::DirectoryListing, "lib/gen")),
                SkyItem::Node(name(Kind::FileState, "lib/gen/out.txt")),
            ],
            vec![],
        );
        let (m, _) = classify(&d, Some(&from), Some(&to), &set(&["lib/BUILD"]), &errs(&[]));
        assert_eq!(classes(&m), vec![RowClass::CreatedInWindow; 2], "{m:?}");
    }

    /// (xii) The package a path sits in is the nearest `BUILD` above it.
    #[test]
    fn the_enclosing_package_is_the_nearest_build_file_above() {
        let t = tree(&[
            "BUILD.bazel",
            "a/BUILD",
            "a/b/c.txt",
            "a/b/BUILD.bazel",
            "x/y/z.txt",
        ]);
        let s = |v: &str| Some(v.to_string());
        assert_eq!(enclosing_package(&t, "a/b/c.txt"), s("a/b"));
        assert_eq!(enclosing_package(&t, "a/b"), s("a/b"));
        assert_eq!(enclosing_package(&t, "a/other.txt"), s("a"));
        assert_eq!(enclosing_package(&t, "a/sub/BUILD"), s("a"));
        assert_eq!(enclosing_package(&t, "x/y/z.txt"), s(""));
        assert_eq!(enclosing_package(&t, ""), s(""));
        let no_root = tree(&["a/BUILD", "x/y/z.txt"]);
        assert_eq!(enclosing_package(&no_root, "x/y/z.txt"), None);
        assert_eq!(enclosing_package(&no_root, ""), None);
    }

    /// (xiii) What each kind's key names.
    #[test]
    fn every_kind_locates_what_its_key_names() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(locate(&name(Kind::File, "a/b.txt")), (s("a/b.txt"), None));
        assert_eq!(
            locate(&name(Kind::FileState, "a/BUILD")),
            (s("a/BUILD"), None)
        );
        assert_eq!(locate(&name(Kind::DirectoryListing, "a")), (s("a"), None));
        assert_eq!(
            locate(&name(Kind::DirectoryListingState, "a/b")),
            (s("a/b"), None)
        );
        let other_root = SkyName {
            kind: Kind::File,
            root: "/somewhere/else".into(),
            key: "a/b.txt".into(),
        };
        assert_eq!(locate(&other_root), (None, None));
        assert_eq!(locate(&name(Kind::Package, "a/b")), (s("a/b"), s("a/b")));
        assert_eq!(locate(&name(Kind::PackageLookup, "")), (s(""), s("")));
        assert_eq!(
            locate(&name(Kind::PackageLookup, "@@rules//x")),
            (None, None)
        );
        assert_eq!(
            locate(&name(
                Kind::Glob,
                "<GlobsKey packageRoot = <workspace>, packageIdentifier = a/b, globRequests = []>"
            )),
            (s("a/b"), s("a/b"))
        );
        assert_eq!(
            locate(&name(
                Kind::Glob,
                "<GlobDescriptor packageName=a/b packageRoot=<workspace> subdir= pattern=*.txt>"
            )),
            (s("a/b"), s("a/b"))
        );
        assert_eq!(
            locate(&name(
                Kind::BzlLoad,
                "KeyForBuild{label=@@//pkg:f.bzl, isBuildPrelude=false}"
            )),
            (s("pkg/f.bzl"), None)
        );
        assert_eq!(
            locate(&name(
                Kind::BzlLoad,
                "KeyForBuild{label=//pkg:f.bzl, isBuildPrelude=false}"
            )),
            (s("pkg/f.bzl"), None)
        );
        assert_eq!(
            locate(&name(
                Kind::BzlLoad,
                "KeyForBuild{label=@@//:f.bzl, isBuildPrelude=false}"
            )),
            (s("f.bzl"), None)
        );
        assert_eq!(
            locate(&name(
                Kind::BzlLoad,
                "KeyForBuild{label=@@rules_cc//cc:defs.bzl, isBuildPrelude=false}"
            )),
            (None, None)
        );
        assert_eq!(locate(&name(Kind::Ignored, "@@")), (None, None));
        assert_eq!(locate(&name(Kind::Tombstone, "gone")), (None, None));
    }

    /// (xiv) The document round-trips through its file, under the versioned
    /// name beside the walk result, sorted unexplained-first.
    #[test]
    fn the_sidecar_round_trips_under_its_versioned_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = RebuildRows::path(dir.path(), "abc123", Some("9.2.0"));
        assert_eq!(path.file_name().unwrap(), "rebuild-abc123-9.2.0.json");
        assert_eq!(
            RebuildRows::path(dir.path(), "abc123", None)
                .file_name()
                .unwrap(),
            "rebuild-abc123.json"
        );
        let d = diff(
            vec![
                SkyItem::Node(name(Kind::PackageLookup, "newdir/sub")),
                SkyItem::Node(name(Kind::File, "lib/a.cc")),
                SkyItem::Edge {
                    from: name(Kind::Package, "lib"),
                    to: name(Kind::File, "lib/a.cc"),
                },
            ],
            vec![SkyItem::Node(name(Kind::File, "old/x.txt"))],
        );
        let (m, e) = in_window(&d, &[], &[]);
        let doc = RebuildRows::new("subj", "abc123", "f0", "abc123", "9.2.0", m, e);
        assert_eq!(
            classes(&doc.missing),
            vec![
                RowClass::Unexplained,
                RowClass::Unexplained,
                RowClass::CreatedInWindow
            ]
        );
        assert_eq!(
            doc.counts,
            RowCounts {
                missing: 3,
                missing_expected: 1,
                missing_unexplained: 2,
                extra: 1,
                extra_expected: 1,
            }
        );
        assert!(!doc.truncated);
        doc.write(&path).unwrap();
        let back = RebuildRows::read(&path).unwrap();
        assert_eq!(back, doc);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"item\": \"edge\""), "{text}");
        assert!(text.contains("\"class\": \"created_in_window\""), "{text}");
    }

    #[test]
    fn a_directory_exists_through_any_file_under_it() {
        let t = tree(&["a/b/c.txt", "a b/d.txt", "z"]);
        assert!(exists(&t, ""));
        assert!(exists(&t, "a"));
        assert!(exists(&t, "a/b"));
        assert!(exists(&t, "a/b/c.txt"));
        assert!(exists(&t, "a b"));
        assert!(!exists(&t, "a/b/c"));
        assert!(!exists(&t, "a/"));
        assert!(!exists(&t, "b"));
        assert!(exists(&t, "z"));
        // A file is not a directory; the root is.
        assert!(is_dir(&t, ""));
        assert!(is_dir(&t, "a/b"));
        assert!(!is_dir(&t, "a/b/c.txt"));
        assert!(!is_dir(&t, "z"));
        assert!(!is_dir(&t, "nowhere"));
    }
}
