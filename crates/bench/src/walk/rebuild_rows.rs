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
//! repository path and package it names when one can be read off the key,
//! and a [`RowClass`]. The classes are decided from three things the walk
//! has to hand: the subject's tree at the classification base and at the
//! compared commit (`git ls-tree`), the applied layer itself, and the two
//! indexes' `error_packages`.
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
//! shortfall, not a `//...`-only row.
//!
//! A package that failed to load under `--keep_going` on one side and not
//! the other has different rows on each: [`RowClass::LoadError`]. Everything
//! else missing is [`RowClass::Unexplained`], and that count is what the
//! oracle's verdict is built from.
//!
//! Extras never fail the oracle. A refresh cannot remove a node, so a file,
//! package or `.bzl` that stopped existing leaves its row behind:
//! [`RowClass::DeletedInWindow`] when the path is gone at the compared
//! commit, [`RowClass::Other`] when it is not (a `load()` that was dropped,
//! a glob that stopped matching).

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
    /// `created_in_window` plus `load_error`.
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
    /// The package the key names, when it names one.
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
    /// Its package failed to load on one side or the other.
    LoadError,
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
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// The tallies over two classified lists.
pub fn counts(missing: &[Row], extra: &[Row]) -> RowCounts {
    let of = |rows: &[Row], class: RowClass| rows.iter().filter(|r| r.class == class).count();
    RowCounts {
        missing: missing.len(),
        missing_expected: of(missing, RowClass::CreatedInWindow) + of(missing, RowClass::LoadError),
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
    if p.is_empty() || tree.contains(p) {
        return true;
    }
    let dir = format!("{p}/");
    tree.range(dir.clone()..)
        .next()
        .is_some_and(|f| f.starts_with(&dir))
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

/// The packages either index says failed to load.
pub fn error_packages(applied: IndexRef<'_>, rebuilt: IndexRef<'_>) -> BTreeSet<String> {
    applied
        .error_packages
        .iter()
        .chain(rebuilt.error_packages.iter())
        .cloned()
        .collect()
}

/// Classifies every row of a diff.
///
/// `from` and `to` are the trees at the classification base and at the
/// compared commit; with either absent nothing is expected, every missing
/// row is unexplained and every extra is `other`. `applied_paths` is
/// [`applied_paths`] over the applied layer, `error_packages` the union of
/// both indexes'.
pub fn classify(
    diff: &SkyDiff,
    from: Option<&BTreeSet<String>>,
    to: Option<&BTreeSet<String>>,
    applied_paths: &BTreeSet<String>,
    error_packages: &BTreeSet<String>,
) -> (Vec<Row>, Vec<Row>) {
    let trees = from.zip(to);
    let missing = diff
        .missing
        .iter()
        .map(|item| {
            row(
                item,
                missing_class(item, trees, applied_paths, error_packages),
            )
        })
        .collect();
    let extra = diff
        .extra
        .iter()
        .map(|item| row(item, extra_class(item, to, error_packages)))
        .collect();
    (missing, extra)
}

fn row(item: &SkyItem, class: RowClass) -> Row {
    let (path, package) = located(item);
    Row {
        item: item.clone(),
        path,
        package,
        class,
    }
}

/// An edge is located by its `to` end (the dependency that is absent),
/// falling back to `from`.
fn located(item: &SkyItem) -> (Option<String>, Option<String>) {
    match item {
        SkyItem::Node(n) => locate(n),
        SkyItem::Edge { from, to } => match locate(to) {
            (None, None) => locate(from),
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
    error_packages: &BTreeSet<String>,
) -> RowClass {
    endpoints(item)
        .into_iter()
        .map(|n| missing_node_class(n, trees, applied_paths, error_packages))
        .max_by_key(|c| match c {
            RowClass::CreatedInWindow => 2,
            RowClass::LoadError => 1,
            _ => 0,
        })
        .unwrap_or(RowClass::Unexplained)
}

fn missing_node_class(
    name: &SkyName,
    trees: Trees<'_>,
    applied_paths: &BTreeSet<String>,
    error_packages: &BTreeSet<String>,
) -> RowClass {
    let Some((from, to)) = trees else {
        return RowClass::Unexplained;
    };
    let (path, package) = locate(name);
    if let Some(root) = path.as_deref().and_then(|p| created_root(p, from, to)) {
        if !exists(applied_paths, &root) {
            return RowClass::CreatedInWindow;
        }
    }
    if package.is_some_and(|p| error_packages.contains(&p)) {
        return RowClass::LoadError;
    }
    RowClass::Unexplained
}

/// The directory created inside the window that `p` sits under: the
/// shallowest prefix, absent at `from`, of the deepest ancestor of `p` that
/// exists at `to` (`p` itself when it does; a `BUILD` probe of a directory
/// with no `BUILD` resolves through the directory). `None` when that
/// ancestor already existed at `from`, or when `p` has none.
fn created_root(p: &str, from: &BTreeSet<String>, to: &BTreeSet<String>) -> Option<String> {
    let mut anchor = p;
    while !exists(to, anchor) {
        anchor = anchor.rsplit_once('/').map_or("", |(dir, _)| dir);
    }
    if anchor.is_empty() {
        return None;
    }
    anchor
        .match_indices('/')
        .map(|(i, _)| i)
        .chain(std::iter::once(anchor.len()))
        .map(|i| &anchor[..i])
        .find(|prefix| !exists(from, prefix))
        .map(str::to_string)
}

/// An extra is deleted-in-window when an end names a path gone at `to`, a
/// load error when an end's package failed, else other. With no tree,
/// other.
fn extra_class(
    item: &SkyItem,
    to: Option<&BTreeSet<String>>,
    error_packages: &BTreeSet<String>,
) -> RowClass {
    let Some(to) = to else {
        return RowClass::Other;
    };
    let ends: Vec<(Option<String>, Option<String>)> =
        endpoints(item).into_iter().map(locate).collect();
    if ends
        .iter()
        .any(|(p, _)| p.as_deref().is_some_and(|p| !exists(to, p)))
    {
        return RowClass::DeletedInWindow;
    }
    if ends
        .iter()
        .any(|(_, pkg)| pkg.as_deref().is_some_and(|p| error_packages.contains(p)))
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
        "newdir/BUILD",
        "newdir/sub/f.txt",
        "lib/old.bzl",
    ];

    /// (i) A node under a directory the window created, with nothing of the
    /// applied layer under it, is a `//...`-only row.
    #[test]
    fn a_node_under_a_new_directory_the_applied_layer_never_saw_is_expected() {
        let d = diff(
            vec![SkyItem::Node(name(Kind::PackageLookup, "newdir/sub"))],
            vec![],
        );
        let (m, _) = classify(
            &d,
            Some(&tree(FROM)),
            Some(&tree(TO)),
            &set(&["lib"]),
            &set(&[]),
        );
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
        let (m, _) = classify(
            &d,
            Some(&tree(FROM)),
            Some(&tree(TO)),
            &set(&["newdir/BUILD"]),
            &set(&[]),
        );
        assert_eq!(classes(&m), vec![RowClass::Unexplained]);
    }

    /// (iii) A path that existed at the base had every chance to be written.
    #[test]
    fn a_path_present_at_the_base_is_unexplained() {
        let d = diff(vec![SkyItem::Node(name(Kind::File, "lib/a.cc"))], vec![]);
        let (m, _) = classify(&d, Some(&tree(FROM)), Some(&tree(TO)), &set(&[]), &set(&[]));
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
        let (m, _) = classify(
            &d,
            Some(&tree(FROM)),
            Some(&tree(TO)),
            &set(&[]),
            &set(&["lib"]),
        );
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
        let (m, _) = classify(&d, Some(&tree(FROM)), Some(&tree(TO)), &set(&[]), &set(&[]));
        assert_eq!(classes(&m), vec![RowClass::CreatedInWindow]);
        // But a probe of a directory that exists nowhere resolves to nothing.
        let d = diff(
            vec![SkyItem::Node(name(Kind::FileState, "nowhere/BUILD"))],
            vec![],
        );
        let (m, _) = classify(&d, Some(&tree(FROM)), Some(&tree(TO)), &set(&[]), &set(&[]));
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
        let (m, _) = classify(
            &d,
            Some(&tree(FROM)),
            Some(&tree(TO)),
            &set(&["lib"]),
            &set(&[]),
        );
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
        let (m, _) = classify(
            &d,
            Some(&tree(FROM)),
            Some(&tree(TO)),
            &set(&[]),
            &set(&["lib"]),
        );
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
        let (_, e) = classify(&d, Some(&tree(FROM)), Some(&tree(TO)), &set(&[]), &set(&[]));
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
        let (m, e) = classify(&d, None, None, &set(&[]), &set(&["lib"]));
        assert_eq!(
            classes(&m),
            vec![RowClass::Unexplained, RowClass::Unexplained]
        );
        assert_eq!(classes(&e), vec![RowClass::Other]);
        let (m, _) = classify(&d, Some(&tree(FROM)), None, &set(&[]), &set(&[]));
        assert_eq!(
            classes(&m),
            vec![RowClass::Unexplained, RowClass::Unexplained]
        );
    }

    /// (ix) What each kind's key names.
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

    /// (x) The document round-trips through its file, under the versioned
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
        let (m, e) = classify(&d, Some(&tree(FROM)), Some(&tree(TO)), &set(&[]), &set(&[]));
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
    }
}
