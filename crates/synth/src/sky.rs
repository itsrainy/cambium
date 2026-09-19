//! A `bazel dump --skyframe=deps` for a tree that has no Bazel behind it.
//!
//! The engine's dirty stage walks the Skyframe loading layer
//! (`cambium_index::sky`), so a row source that cannot produce one builds an
//! index no run can be answered from. The row sources the tests use are all of
//! that kind: a synthetic graph, a map of path to text, a directory of toy
//! BUILD files. This writes the dump such a tree would have produced.
//!
//! It is a *model* of Bazel's loading phase, which is the thing the engine
//! itself is no longer allowed to be. That is the point of putting it here:
//! the model lives in the test harness, where being wrong makes a test wrong,
//! rather than in the engine, where being wrong makes an answer wrong. The
//! shapes it writes are the ones `crates/index/src/sky/fixtures/*.dump` hold
//! verbatim from real Bazel, and the layer's own checks run against those.
//!
//! What it models, and how it errs:
//!
//! - every package depends on its `PACKAGE_LOOKUP`, its `BUILD` file, the
//!   ignore node, the `BZL_LOAD` of every `.bzl` its BUILD file loads, its
//!   glob node, and the `PACKAGE_LOOKUP` of every directory below it. Real
//!   Bazel looks up only the directories the package actually reaches into;
//!   looking up all of them dirties a parent slightly more often than Bazel
//!   would, which is the safe direction.
//! - the glob node depends on the directory listing of the package and of
//!   every directory below it, as a recursive `glob(["**"])` does.
//! - `BZL_LOAD` nodes depend on the file and on the `BZL_LOAD` of what they
//!   load, which is the edge a `.bzl` loaded only by other `.bzl` files needs.

use std::collections::{BTreeMap, BTreeSet};

use cambium_index::loads::{resolve_label, scan_loads, Load};
use cambium_index::{FileText, RepoMapping};

/// Builds the dump for a tree.
///
/// `packages` are the package names (`""` is the root package), `files` every
/// path in the tree with its text. The workspace root is written as
/// `/workspace`, which is also what [`paths`] erases.
pub fn dump(packages: &[String], files: &dyn Tree) -> String {
    Dump::new(packages, files).render()
}

/// The paths a dump from [`dump`] is normalised against.
pub fn paths() -> cambium_index::sky::Paths {
    cambium_index::sky::Paths::new(ROOT, "/output_base")
}

const ROOT: &str = "/workspace";

/// What the dump needs to know about a tree: which paths exist, and the text
/// of the Starlark ones.
pub trait Tree {
    fn paths(&self) -> Vec<String>;
    fn text(&self, path: &str) -> Option<String>;
}

/// A tree from a map of path to text, which is what the engine's in-memory
/// test queriers hold.
pub struct InMemory<'a>(pub &'a std::collections::BTreeMap<String, String>);

impl Tree for InMemory<'_> {
    fn paths(&self) -> Vec<String> {
        self.0.keys().cloned().collect()
    }

    fn text(&self, path: &str) -> Option<String> {
        self.0.get(path).cloned()
    }
}

/// A tree from an explicit path list and a [`FileText`], which is what the
/// synthetic graph's own workspace is.
pub struct Listed<'a> {
    pub paths: Vec<String>,
    pub text: &'a dyn FileText,
}

impl Tree for Listed<'_> {
    fn paths(&self) -> Vec<String> {
        self.paths.clone()
    }

    fn text(&self, path: &str) -> Option<String> {
        self.text.text(path)
    }
}

/// A tree read off the filesystem, for the directory-backed queriers.
pub struct OnDisk(pub std::path::PathBuf);

impl Tree for OnDisk {
    fn paths(&self) -> Vec<String> {
        let mut out = Vec::new();
        walk(&self.0, &self.0, &mut out);
        out.sort();
        out
    }

    fn text(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.0.join(path)).ok()
    }
}

fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        if p.is_dir() {
            walk(root, &p, out);
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

struct Dump {
    /// Key -> its dependencies, in the dump's own spelling.
    blocks: BTreeMap<String, BTreeSet<String>>,
}

impl Dump {
    fn new(packages: &[String], tree: &dyn Tree) -> Self {
        let mut d = Dump {
            blocks: BTreeMap::new(),
        };
        let files = tree.paths();
        let dirs = directories(&files);
        let mapping = RepoMapping::identity();

        // Every file and directory: the `FILE -> FILE_STATE` pair Bazel keeps,
        // and the listing pair for a directory.
        for f in &files {
            d.file(f);
        }
        for dir in &dirs {
            d.file(dir);
            d.edge(
                &rooted("DIRECTORY_LISTING", dir),
                rooted("DIRECTORY_LISTING_STATE", dir),
            );
            d.node(rooted("DIRECTORY_LISTING_STATE", dir));
        }

        // `.bazelignore` is the ignore node's one input, which is what makes
        // an edit to it dirty every package.
        d.edge("IGNORED_SUBDIRECTORIES:@@", rooted("FILE", ".bazelignore"));

        // Every Starlark file that anything loads gets a `BZL_LOAD` node.
        for f in &files {
            if !f.ends_with(".bzl") {
                continue;
            }
            let key = bzl_key(f, false);
            d.edge(&key, rooted("FILE", f));
            for loaded in loads_of(tree, f, &mapping) {
                d.edge(&key, bzl_key(&loaded, false));
            }
        }

        // The workspace closure gets a second set of nodes under the key the
        // module system uses. That is how Bazel says a `.bzl` can define
        // repository rules — `KeyForBzlmod` on 8/9, `KeyForWorkspace` on 7 —
        // and it is what the engine reads instead of a flag on a stored
        // closure.
        for f in workspace_closure(tree, &files, &mapping) {
            let key = bzl_key(&f, true);
            d.edge(&key, rooted("FILE", &f));
            for loaded in loads_of(tree, &f, &mapping) {
                d.edge(&key, bzl_key(&loaded, true));
            }
        }

        for p in packages {
            let pkg_key = format!("PACKAGE:{p}");
            let lookup = format!("PACKAGE_LOOKUP:{p}");
            d.edge(&pkg_key, lookup.clone());
            d.edge(&pkg_key, "IGNORED_SUBDIRECTORIES:@@".to_string());
            for name in ["BUILD", "BUILD.bazel"] {
                let path = join(p, name);
                // The `FILE -> FILE_STATE` pair exists whether or not the file
                // does: Bazel asks the filesystem either way, and "there is
                // nothing here" is the answer a later commit invalidates by
                // putting a `BUILD` file there. A real dump carries the pair
                // for `a/sub/BUILD` on a tree that has no such file.
                d.file(&path);
                d.edge(&lookup, rooted("FILE", &path));
                if files.contains(&path) {
                    d.edge(&pkg_key, rooted("FILE", &path));
                    for loaded in loads_of(tree, &path, &mapping) {
                        d.edge(&pkg_key, bzl_key(&loaded, false));
                    }
                }
            }
            // The glob node, and the listings it reads.
            let glob = format!(
                "GLOBS:<GlobsKey packageRoot = {ROOT}, packageIdentifier = {p}, \
                 globRequests = [GlobRequest: ** FILES]>"
            );
            d.edge(&pkg_key, glob.clone());
            for dir in &dirs {
                if under(p, dir) {
                    d.edge(&glob, rooted("DIRECTORY_LISTING", dir));
                    // The lookup that tells Bazel whether the directory has
                    // become a package of its own: the edge a new `BUILD`
                    // file carving a sub-package out of its parent travels.
                    if dir != p {
                        d.edge(&pkg_key, format!("PACKAGE_LOOKUP:{dir}"));
                        for name in ["BUILD", "BUILD.bazel"] {
                            let path = join(dir, name);
                            d.file(&path);
                            d.edge(&format!("PACKAGE_LOOKUP:{dir}"), rooted("FILE", &path));
                        }
                    }
                }
            }
        }
        d
    }

    fn node(&mut self, key: String) {
        self.blocks.entry(key).or_default();
    }

    fn edge(&mut self, from: &str, to: String) {
        self.blocks.entry(to.clone()).or_default();
        self.blocks.entry(from.to_string()).or_default().insert(to);
    }

    /// `FILE:[root]/[p] -> FILE_STATE:[root]/[p]`, and the parent directory's
    /// `FILE` node above it, exactly as Bazel writes them.
    fn file(&mut self, path: &str) {
        let key = rooted("FILE", path);
        self.edge(&key, rooted("FILE_STATE", path));
        if let Some(i) = path.rfind('/') {
            self.edge(&key, rooted("FILE", &path[..i]));
        }
        self.node(rooted("FILE_STATE", path));
    }

    fn render(&self) -> String {
        let mut out = String::from(
            "WARNING: This information is intended for consumption by developers only.\n",
        );
        for (key, deps) in &self.blocks {
            out.push_str(key);
            out.push('\n');
            for (i, dep) in deps.iter().enumerate() {
                out.push_str(&format!("  Group {}:\n    {dep}\n\n", i + 1));
            }
            out.push('\n');
        }
        out
    }
}

fn rooted(kind: &str, rel: &str) -> String {
    format!("{kind}:[{ROOT}]/[{rel}]")
}

fn bzl_key(path: &str, workspace_level: bool) -> String {
    let (pkg, name) = match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    };
    if workspace_level {
        format!("BZL_LOAD:KeyForBzlmod{{label=//{pkg}:{name}, isBuildPrelude=false}}")
    } else {
        format!("BZL_LOAD:KeyForBuild{{label=//{pkg}:{name}, isBuildPrelude=false}}")
    }
}

/// The main-repository `.bzl` files `MODULE.bazel` / `WORKSPACE` reach,
/// transitively. These are the ones that can define repository rules.
fn workspace_closure(tree: &dyn Tree, files: &[String], m: &RepoMapping) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = Vec::new();
    for name in cambium_index::loads::WORKSPACE_FILES {
        if !files.contains(&(*name).to_string()) {
            continue;
        }
        let Some(text) = tree.text(name) else {
            continue;
        };
        for l in cambium_index::loads::scan_workspace_loads(&text) {
            if let Some(Load::Main(p)) = resolve_label(&l, "", m) {
                stack.push(p);
            }
        }
    }
    while let Some(p) = stack.pop() {
        if !out.insert(p.clone()) {
            continue;
        }
        stack.extend(loads_of(tree, &p, m));
    }
    out
}

fn join(pkg: &str, name: &str) -> String {
    if pkg.is_empty() {
        name.to_string()
    } else {
        format!("{pkg}/{name}")
    }
}

/// Whether `dir` is `pkg` or below it. The root package is above everything.
fn under(pkg: &str, dir: &str) -> bool {
    pkg.is_empty() || dir == pkg || dir.starts_with(&format!("{pkg}/"))
}

/// Every directory the paths imply, the root included as the empty string.
fn directories(files: &[String]) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = [String::new()].into();
    for f in files {
        let mut d = f.as_str();
        while let Some(i) = d.rfind('/') {
            d = &d[..i];
            out.insert(d.to_string());
        }
    }
    out
}

/// The main-repository `.bzl` paths a Starlark file loads.
fn loads_of(tree: &dyn Tree, path: &str, m: &RepoMapping) -> Vec<String> {
    let Some(text) = tree.text(path) else {
        return Vec::new();
    };
    let from_pkg = cambium_index::loads::owning_dir(path);
    scan_loads(&text)
        .into_iter()
        .filter_map(|l| match resolve_label(&l, &from_pkg, m) {
            Some(Load::Main(p)) => Some(p),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cambium_index::sky::{seeds, Kind, SkyLayer};

    fn tree() -> std::collections::BTreeMap<String, String> {
        [
            ("BUILD", ""),
            ("a/BUILD", "load(\":rules.bzl\", \"r\")"),
            ("a/rules.bzl", "load(\":helper.bzl\", \"h\")"),
            ("a/helper.bzl", ""),
            ("a/one.cc", ""),
            ("a/sub/x.cc", ""),
            (".bazelignore", ""),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn layer(packages: &[&str]) -> cambium_index::SkyTables {
        let pkgs: Vec<String> = packages.iter().map(|s| s.to_string()).collect();
        let mut l = SkyLayer::new();
        let files = tree();
        let seen = l.apply_dump(&dump(&pkgs, &InMemory(&files)), &paths());
        assert!(seen > 10, "{seen} blocks");
        l.finish()
    }

    #[test]
    fn the_synthetic_dump_carries_the_chains_the_dirty_stage_walks() {
        let t = layer(&["", "a"]);
        let v = t.as_ref();

        // A new BUILD file carving a sub-package dirties the parent.
        let w = v.walk(&seeds([("a/sub/BUILD", true)]));
        assert!(w.packages.contains("a"), "{w:?}");

        // A `.bzl` loaded only by another `.bzl`.
        let w = v.walk(&seeds([("a/helper.bzl", false)]));
        assert!(w.packages.contains("a"), "{w:?}");
        assert!(w.bzl_packages.contains("a"), "{w:?}");

        // A file added where a glob can see it.
        let w = v.walk(&seeds([("a/two.cc", true)]));
        assert!(w.packages.contains("a"), "{w:?}");

        // A content change moves nothing here.
        let w = v.walk(&seeds([("a/one.cc", false)]));
        assert!(w.packages.is_empty(), "{w:?}");

        // `.bazelignore` reaches every package.
        let w = v.walk(&seeds([(".bazelignore", false)]));
        assert_eq!(w.packages, ["".to_string(), "a".to_string()].into());
    }

    #[test]
    fn the_dump_it_writes_parses_as_the_real_thing_does() {
        let t = layer(&["", "a"]);
        let v = t.as_ref();
        for (kind, text) in [
            (Kind::Package, "a"),
            (Kind::PackageLookup, "a/sub"),
            (Kind::FileState, "a/rules.bzl"),
            (Kind::DirectoryListingState, "a/sub"),
        ] {
            assert!(v.id_of(kind, text).is_some(), "{} {text}", kind.name());
        }
    }
}
