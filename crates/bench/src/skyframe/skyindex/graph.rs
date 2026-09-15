//! The index: Skyframe's own loading-phase node kinds, plus a rule layer.
//!
//! Two layers, because the dump holds exactly one of them.
//!
//! The **dump layer** is `bazel dump --skyframe=deps` with the kinds that
//! carry a loading-phase dependency kept and everything else dropped:
//! `FILE_STATE`, `FILE`, `DIRECTORY_LISTING_STATE`, `DIRECTORY_LISTING`,
//! `GLOBS`, `PACKAGE_LOOKUP`, `PACKAGE` and `BZL_LOAD`. The dropped kinds are
//! not an omission of convenience: `PRECOMPUTED:*`, `IGNORED_SUBDIRECTORIES`,
//! `STARLARK_BUILTINS` and `REPOSITORY_MAPPING` are singletons every package
//! in the repository depends on, so keeping them would put every package one
//! reverse hop from every other and the walk would answer `//...` every time.
//!
//! The **rule layer** is the targets, which the loading-phase dump does not
//! have: on Bazel 9.2 a `bazel query //...` leaves no `TARGET` keys behind
//! (see SKYFRAME_PROTOTYPES.md), so rule labels, the package each belongs to,
//! and the labels each depends on come from the `//...:*` proto stream. The
//! two layers meet at the package: a `PACKAGE` node the dump layer dirties
//! dirties every rule the rule layer files under it.
//!
//! Nothing here classifies a change. The only inputs are the changed paths;
//! everything after that is a reverse walk over stored edges.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use serde::{Deserialize, Serialize};

use super::dump::{kind_of, parse_dump, split_rooted};

/// A Skyframe node kind the index keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Kind {
    FileState,
    File,
    DirectoryListingState,
    DirectoryListing,
    Globs,
    PackageLookup,
    Package,
    BzlLoad,
}

impl Kind {
    /// The kind a dump key names, or `None` for a kind the index drops.
    pub fn from_dump(kind: &str) -> Option<Self> {
        Some(match kind {
            "FILE_STATE" => Kind::FileState,
            "FILE" => Kind::File,
            "DIRECTORY_LISTING_STATE" => Kind::DirectoryListingState,
            "DIRECTORY_LISTING" => Kind::DirectoryListing,
            "GLOBS" => Kind::Globs,
            "PACKAGE_LOOKUP" => Kind::PackageLookup,
            "PACKAGE" => Kind::Package,
            "BZL_LOAD" => Kind::BzlLoad,
            _ => return None,
        })
    }

    /// Whether this kind's key is written as `[root]/[relative/path]`.
    fn is_rooted(self) -> bool {
        matches!(
            self,
            Kind::FileState | Kind::File | Kind::DirectoryListingState | Kind::DirectoryListing
        )
    }
}

/// The root a rooted key is under. `0` is always the subject's workspace, so
/// a stored graph names no absolute path of the machine that built it.
type RootId = u32;

/// The workspace root's id.
pub const WORKSPACE: RootId = 0;

/// One rule, as the rule layer holds it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleNode {
    /// The package the rule is declared in, as the dump spells packages.
    pub package: String,
    /// Labels of other rules in the repository this rule depends on.
    pub deps: Vec<String>,
    /// Workspace-relative paths of the source files this rule depends on.
    ///
    /// This edge has no counterpart in the loading-phase dump, and that is
    /// one of the prototype's findings rather than a shortcut: editing the
    /// *contents* of a `.cc` changes no loading-phase key at all, so a walk
    /// over the dump alone reaches nothing. The dependency is real, and it is
    /// in the proto stream, so the rule layer carries it.
    pub file_deps: Vec<String>,
}

/// The stored index.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkyIndex {
    /// Roots, `roots[0]` being the workspace and written as the empty string.
    pub roots: Vec<String>,
    /// The dump layer's nodes, in insertion order.
    pub nodes: Vec<(Kind, RootId, String)>,
    /// Reverse dependencies, CSR: `rdeps[offsets[i]..offsets[i + 1]]` are the
    /// nodes that depend on node `i`.
    pub offsets: Vec<u32>,
    pub rdeps: Vec<u32>,
    /// The rule layer, by label.
    pub rules: BTreeMap<String, RuleNode>,
}

/// Builds a [`SkyIndex`] from a dump and a proto stream.
pub struct Builder {
    workspace: String,
    roots: Vec<String>,
    root_ids: HashMap<String, RootId>,
    nodes: Vec<(Kind, RootId, String)>,
    ids: HashMap<(Kind, RootId, String), u32>,
    fwd: Vec<Vec<u32>>,
    rules: BTreeMap<String, RuleNode>,
}

impl Builder {
    /// `workspace` is the absolute path of the subject's worktree, which is
    /// the one root that gets erased.
    pub fn new(workspace: &str) -> Self {
        Self {
            workspace: workspace.trim_end_matches('/').to_string(),
            roots: vec![String::new()],
            root_ids: HashMap::new(),
            nodes: Vec::new(),
            ids: HashMap::new(),
            fwd: Vec::new(),
            rules: BTreeMap::new(),
        }
    }

    fn root_id(&mut self, root: &str) -> RootId {
        if root.trim_end_matches('/') == self.workspace {
            return WORKSPACE;
        }
        if let Some(&id) = self.root_ids.get(root) {
            return id;
        }
        let id = self.roots.len() as RootId;
        self.roots.push(root.to_string());
        self.root_ids.insert(root.to_string(), id);
        id
    }

    /// The `(kind, root, text)` a dump key names, or `None` for a dropped kind.
    fn normalize(&mut self, key: &str) -> Option<(Kind, RootId, String)> {
        let (kind_str, rest) = kind_of(key)?;
        let kind = Kind::from_dump(kind_str)?;
        if kind.is_rooted() {
            let (root, rel) = split_rooted(rest)?;
            let root = self.root_id(root);
            return Some((kind, root, rel.to_string()));
        }
        // `GLOBS` and `BZL_LOAD` keys are structured text that embeds the
        // package root, so the workspace path is erased there too; `PACKAGE`
        // and `PACKAGE_LOOKUP` keys are already just a package name.
        let text = if self.workspace.is_empty() {
            rest.to_string()
        } else {
            rest.replace(&self.workspace, "<workspace>")
        };
        Some((kind, WORKSPACE, text))
    }

    fn intern(&mut self, node: (Kind, RootId, String)) -> u32 {
        if let Some(&id) = self.ids.get(&node) {
            return id;
        }
        let id = self.nodes.len() as u32;
        self.nodes.push(node.clone());
        self.fwd.push(Vec::new());
        self.ids.insert(node, id);
        id
    }

    /// Adds every block of one `bazel dump --skyframe=deps`. Returns how many
    /// blocks the dump held, kept or not.
    pub fn add_dump(&mut self, text: &str) -> usize {
        // Two passes over the borrowed text are not possible while `self` is
        // borrowed mutably by the sink, so collect first. The dump of the
        // `bazel` subject is a few megabytes.
        let mut blocks: Vec<(String, Vec<String>)> = Vec::new();
        parse_dump(text, &mut |k, d| {
            blocks.push((k.to_string(), d.iter().map(|s| s.to_string()).collect()))
        });
        let seen = blocks.len();
        for (key, deps) in blocks {
            let Some(node) = self.normalize(&key) else {
                continue;
            };
            let id = self.intern(node);
            for dep in deps {
                let Some(d) = self.normalize(&dep) else {
                    continue;
                };
                let d = self.intern(d);
                self.fwd[id as usize].push(d);
            }
        }
        seen
    }

    /// Files one rule under its package.
    pub fn add_rule(&mut self, label: String, node: RuleNode) {
        self.rules.insert(label, node);
    }

    pub fn finish(self) -> SkyIndex {
        let n = self.nodes.len();
        let mut counts = vec![0u32; n + 1];
        for deps in &self.fwd {
            for &d in deps {
                counts[d as usize + 1] += 1;
            }
        }
        let mut offsets = counts;
        for i in 1..offsets.len() {
            offsets[i] += offsets[i - 1];
        }
        let mut cursor = offsets.clone();
        let mut rdeps = vec![0u32; offsets[n] as usize];
        for (from, deps) in self.fwd.iter().enumerate() {
            for &d in deps {
                let slot = &mut cursor[d as usize];
                rdeps[*slot as usize] = from as u32;
                *slot += 1;
            }
        }
        SkyIndex {
            roots: self.roots,
            nodes: self.nodes,
            offsets,
            rdeps,
            rules: self.rules,
        }
    }
}

/// What one answer walked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WalkOutcome {
    /// Packages the dump layer's reverse walk reached.
    pub dirty_packages: BTreeSet<String>,
    /// Seeds that named no node in the graph, which is normal: a path the
    /// loading phase never looked at has no `FILE_STATE` key.
    pub unknown_seeds: usize,
    /// Rules reached, as labels.
    pub targets: BTreeSet<String>,
}

impl SkyIndex {
    /// A lookup table from `(kind, text)` under the workspace root to node id.
    fn workspace_ids(&self) -> HashMap<(Kind, &str), u32> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, (_, r, _))| *r == WORKSPACE)
            .map(|(i, (k, _, t))| ((*k, t.as_str()), i as u32))
            .collect()
    }

    /// The dirty seed a set of changed paths makes, as Skyframe keys.
    ///
    /// Every changed path is a `FILE_STATE`. A path that was added or deleted
    /// also changes the listing of the directory holding it, and of every
    /// ancestor directory, since an add may have created the whole chain and
    /// a delete may have emptied it; those are `DIRECTORY_LISTING_STATE`.
    pub fn seeds(changed: &[super::super::ChangedPath]) -> BTreeSet<(Kind, String)> {
        let mut out = BTreeSet::new();
        for c in changed {
            out.insert((Kind::FileState, c.path.clone()));
            // `M` is a content change and moves no listing. Anything else
            // (`A`, `D`, `R`, `C`) adds or removes a name.
            if c.status.starts_with('M') {
                continue;
            }
            let mut dir = c.path.as_str();
            loop {
                dir = match dir.rfind('/') {
                    Some(i) => &dir[..i],
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

    /// The generic answer: dirty the seed, walk reverse dependencies over
    /// every kept kind, take every rule the reached packages hold and every
    /// rule that names a changed file, then close over rule dependencies.
    ///
    /// There is no case analysis anywhere in here, which is the whole point
    /// of the prototype.
    pub fn answer(&self, changed: &[super::super::ChangedPath]) -> WalkOutcome {
        let ids = self.workspace_ids();
        let seeds = Self::seeds(changed);
        let mut unknown_seeds = 0;
        let mut queue: VecDeque<u32> = VecDeque::new();
        let mut seen = vec![false; self.nodes.len()];
        for (kind, text) in &seeds {
            match ids.get(&(*kind, text.as_str())) {
                Some(&id) => {
                    if !seen[id as usize] {
                        seen[id as usize] = true;
                        queue.push_back(id);
                    }
                }
                None => unknown_seeds += 1,
            }
        }
        let mut dirty_packages = BTreeSet::new();
        while let Some(id) = queue.pop_front() {
            let (kind, _, text) = &self.nodes[id as usize];
            // A `PACKAGE_LOOKUP` counts as well as a `PACKAGE`. The two are
            // one kind-level rule, not a case: a package created between the
            // two revisions has no `PACKAGE` node in the index built at the
            // parent, but the lookup that found nothing there does have one,
            // and it carries the same name.
            if matches!(kind, Kind::Package | Kind::PackageLookup) {
                dirty_packages.insert(text.clone());
            }
            let (lo, hi) = (
                self.offsets[id as usize] as usize,
                self.offsets[id as usize + 1] as usize,
            );
            for &up in &self.rdeps[lo..hi] {
                if !seen[up as usize] {
                    seen[up as usize] = true;
                    queue.push_back(up);
                }
            }
        }

        let changed_paths: BTreeSet<&str> = changed.iter().map(|c| c.path.as_str()).collect();
        let targets = self.close_over_rules(&dirty_packages, &changed_paths);
        WalkOutcome {
            dirty_packages,
            unknown_seeds,
            targets,
        }
    }

    /// Every rule in a dirty package or naming a changed file, plus
    /// everything that depends on one, transitively.
    pub fn close_over_rules(
        &self,
        dirty_packages: &BTreeSet<String>,
        changed: &BTreeSet<&str>,
    ) -> BTreeSet<String> {
        let mut consumers: HashMap<&str, Vec<&str>> = HashMap::new();
        for (label, node) in &self.rules {
            for d in &node.deps {
                consumers.entry(d.as_str()).or_default().push(label);
            }
        }
        let mut out: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<&str> = VecDeque::new();
        for (label, node) in &self.rules {
            let seeded = dirty_packages.contains(&node.package)
                || node.file_deps.iter().any(|f| changed.contains(f.as_str()));
            if seeded && out.insert(label.clone()) {
                queue.push_back(label.as_str());
            }
        }
        while let Some(label) = queue.pop_front() {
            let Some(ups) = consumers.get(label) else {
                continue;
            };
            for up in ups {
                if !out.contains(*up) {
                    out.insert((*up).to_string());
                    queue.push_back(up);
                }
            }
        }
        out
    }

    /// Replaces the rule layer's entry for every package in `packages` with
    /// what `fresh` holds, which is what the `-requery` variant does after
    /// re-querying the dirty packages at head.
    pub fn replace_packages(
        &mut self,
        packages: &BTreeSet<String>,
        fresh: BTreeMap<String, RuleNode>,
    ) {
        self.rules.retain(|_, n| !packages.contains(&n.package));
        self.rules.extend(fresh);
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.rdeps.len()
    }

    /// zstd-compressed JSON. The format is not the point; it is small enough
    /// that `state_bytes` compares with the control's index rather than with
    /// the dump's own text size.
    pub fn write(&self, path: &std::path::Path) -> anyhow::Result<u64> {
        let json = serde_json::to_vec(self)?;
        let packed = zstd::encode_all(json.as_slice(), 3)?;
        std::fs::write(path, &packed)?;
        Ok(packed.len() as u64)
    }

    pub fn read(path: &std::path::Path) -> anyhow::Result<Self> {
        let packed = std::fs::read(path)?;
        let json = zstd::decode_all(packed.as_slice())?;
        Ok(serde_json::from_slice(&json)?)
    }
}

/// The workspace-relative path a source label names, or `None` for a label
/// outside the main repository.
pub fn source_path(label: &str) -> Option<String> {
    let rest = label.strip_prefix("//")?;
    let (pkg, name) = rest.split_once(':')?;
    if pkg.is_empty() {
        Some(name.to_string())
    } else {
        Some(format!("{pkg}/{name}"))
    }
}

/// The package a label belongs to, as the dump spells packages.
pub fn label_package(label: &str) -> Option<String> {
    let rest = label.strip_prefix("//")?;
    Some(rest.split_once(':')?.0.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skyframe::ChangedPath;

    /// A dump fixture with the three chains the walk has to follow: a `.bzl`
    /// edit, a `BUILD` edit, and a new sub-package showing up in a glob.
    const DUMP: &str = "\
PACKAGE:a
  Group 1:
    PACKAGE_LOOKUP:a
  Group 2:
    FILE:[/w]/[a/BUILD]
  Group 3:
    BZL_LOAD:KeyForBuild{label=//a:rules.bzl, isBuildPrelude=false}
  Group 4:
    GLOBS:<GlobsKey packageRoot = /w, packageIdentifier = a, globRequests = [GlobRequest: ** FILES]>
  Group 5:
    PRECOMPUTED:autoload_symbols

PACKAGE:a/sub
  Group 1:
    PACKAGE_LOOKUP:a/sub

PACKAGE_LOOKUP:a
  Group 1:
    FILE:[/w]/[a/BUILD]

PACKAGE_LOOKUP:a/sub
  Group 1:
    FILE:[/w]/[a/sub/BUILD]

GLOBS:<GlobsKey packageRoot = /w, packageIdentifier = a, globRequests = [GlobRequest: ** FILES]>
  Group 1:
    DIRECTORY_LISTING:[/w]/[a]
  Group 2:
    PACKAGE_LOOKUP:a/sub

BZL_LOAD:KeyForBuild{label=//a:rules.bzl, isBuildPrelude=false}
  Group 1:
    FILE:[/w]/[a/rules.bzl]

DIRECTORY_LISTING:[/w]/[a]
  Group 1:
    DIRECTORY_LISTING_STATE:[/w]/[a]

FILE:[/w]/[a/BUILD]
  Group 1:
    FILE_STATE:[/w]/[a/BUILD]

FILE:[/w]/[a/sub/BUILD]
  Group 1:
    FILE_STATE:[/w]/[a/sub/BUILD]

FILE:[/w]/[a/rules.bzl]
  Group 1:
    FILE_STATE:[/w]/[a/rules.bzl]

DIRECTORY_LISTING_STATE:[/w]/[a]

FILE_STATE:[/w]/[a/BUILD]

FILE_STATE:[/w]/[a/sub/BUILD]

FILE_STATE:[/w]/[a/rules.bzl]
";

    fn rule(package: &str, deps: &[&str], files: &[&str]) -> RuleNode {
        RuleNode {
            package: package.to_string(),
            deps: deps.iter().map(|s| s.to_string()).collect(),
            file_deps: files.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn index() -> SkyIndex {
        let mut b = Builder::new("/w");
        let seen = b.add_dump(DUMP);
        assert_eq!(seen, 14);
        b.add_rule("//a:lib".into(), rule("a", &[], &["a/lib.cc"]));
        b.add_rule("//a:bin".into(), rule("a", &["//a:lib"], &[]));
        b.add_rule("//a/sub:s".into(), rule("a/sub", &[], &[]));
        b.add_rule("//b:far".into(), rule("b", &["//a:bin"], &[]));
        b.add_rule("//c:alone".into(), rule("c", &[], &[]));
        b.finish()
    }

    fn changed(items: &[(&str, &str)]) -> Vec<ChangedPath> {
        items
            .iter()
            .map(|(s, p)| ChangedPath {
                status: s.to_string(),
                path: p.to_string(),
            })
            .collect()
    }

    #[test]
    fn the_workspace_root_is_erased_and_other_roots_are_kept() {
        let mut b = Builder::new("/w");
        b.add_dump("FILE_STATE:[/w]/[a/x]\n\nFILE_STATE:[/elsewhere]/[b/y]\n");
        let idx = b.finish();
        assert_eq!(idx.roots, vec!["".to_string(), "/elsewhere".to_string()]);
        assert_eq!(idx.nodes[0], (Kind::FileState, WORKSPACE, "a/x".into()));
        assert_eq!(idx.nodes[1], (Kind::FileState, 1, "b/y".into()));
        // The workspace path is gone from a structured key as well.
        let mut b = Builder::new("/w");
        b.add_dump("GLOBS:<GlobsKey packageRoot = /w, packageIdentifier = a>\n");
        assert!(b.finish().nodes[0].2.contains("<workspace>"));
    }

    #[test]
    fn a_kind_the_index_drops_never_becomes_a_node() {
        let idx = index();
        // `PRECOMPUTED:autoload_symbols` is a dep of PACKAGE:a in the fixture.
        assert!(!idx.nodes.iter().any(|(_, _, t)| t == "autoload_symbols"));
    }

    #[test]
    fn a_bzl_edit_reaches_the_packages_that_load_it() {
        let out = index().answer(&changed(&[("M", "a/rules.bzl")]));
        assert_eq!(out.dirty_packages, ["a".to_string()].into());
        // //a:lib and //a:bin are in the package; //b:far depends on //a:bin.
        assert_eq!(
            out.targets,
            ["//a:bin", "//a:lib", "//b:far"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        );
    }

    #[test]
    fn a_new_sub_package_dirties_the_parent_it_carves_out_of() {
        // The dump encodes this as GLOBS:a -> PACKAGE_LOOKUP:a/sub, so the
        // generic walk gets it with no rule about BUILD files anywhere.
        let out = index().answer(&changed(&[("A", "a/sub/BUILD")]));
        assert!(out.dirty_packages.contains("a"), "{out:?}");
        assert!(out.dirty_packages.contains("a/sub"), "{out:?}");
        assert!(out.targets.contains("//a/sub:s"));
        assert!(out.targets.contains("//a:lib"));
        assert!(!out.targets.contains("//c:alone"));
    }

    #[test]
    fn a_directory_listing_moves_when_a_file_is_added_or_deleted() {
        // A new source file under `a` reaches `a`'s glob through the listing.
        let out = index().answer(&changed(&[("A", "a/new.cc")]));
        assert_eq!(out.dirty_packages, ["a".to_string()].into());
        // A pure content change moves no listing, so only the file edge fires.
        let out = index().answer(&changed(&[("M", "a/lib.cc")]));
        assert!(out.dirty_packages.is_empty(), "{out:?}");
        assert_eq!(
            out.targets,
            ["//a:bin", "//a:lib", "//b:far"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        );
    }

    #[test]
    fn a_path_the_loading_phase_never_looked_at_is_counted_not_guessed() {
        let out = index().answer(&changed(&[("M", "README.md")]));
        assert!(out.targets.is_empty(), "{out:?}");
        assert_eq!(out.unknown_seeds, 1);
    }

    #[test]
    fn seeds_are_a_file_state_and_the_listings_an_add_or_delete_moves() {
        let s = SkyIndex::seeds(&changed(&[("M", "a/b/c.txt")]));
        assert_eq!(s, [(Kind::FileState, "a/b/c.txt".to_string())].into());
        let s = SkyIndex::seeds(&changed(&[("D", "a/b/c.txt")]));
        assert!(s.contains(&(Kind::DirectoryListingState, "a/b".to_string())));
        assert!(s.contains(&(Kind::DirectoryListingState, "a".to_string())));
        // A path at the root moves the root directory's own listing.
        let s = SkyIndex::seeds(&changed(&[("A", "TOP")]));
        assert!(s.contains(&(Kind::DirectoryListingState, String::new())));
    }

    #[test]
    fn replacing_a_package_swaps_its_rules_and_keeps_the_rest() {
        let mut idx = index();
        let mut fresh = BTreeMap::new();
        fresh.insert("//a:renamed".to_string(), rule("a", &[], &[]));
        idx.replace_packages(&["a".to_string()].into(), fresh);
        assert!(!idx.rules.contains_key("//a:lib"));
        assert!(idx.rules.contains_key("//a:renamed"));
        assert!(idx.rules.contains_key("//b:far"));
    }

    #[test]
    fn an_index_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skyindex.bin");
        let idx = index();
        let bytes = idx.write(&path).unwrap();
        assert!(bytes > 0);
        assert_eq!(SkyIndex::read(&path).unwrap(), idx);
    }

    #[test]
    fn a_label_names_its_package_and_a_source_label_names_its_path() {
        assert_eq!(source_path("//a/b:c/d.h").as_deref(), Some("a/b/c/d.h"));
        assert_eq!(source_path("//:top.txt").as_deref(), Some("top.txt"));
        assert_eq!(source_path("@@rules_cc+//x:y"), None);
        assert_eq!(label_package("//a/b:c").as_deref(), Some("a/b"));
    }
}
