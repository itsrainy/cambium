//! Parsing `bazel dump --skyframe=deps`.
//!
//! The dump is a flat text rendering of the Skyframe graph. A node key sits at
//! column zero, its dependencies are grouped under `  Group N:` headers at
//! indent two, and each dependency sits at indent four. Blank lines separate
//! groups, separate dependencies within a group, and end a node. A node with no
//! dependencies is a key line followed by a blank line.
//!
//! ```text
//! PACKAGE:src/tools/launcher/util
//!   Group 1:
//!     PRECOMPUTED:lazy_macro_expansion_packages
//!
//!   Group 6:
//!     FILE:[/abs/root]/[src/tools/launcher/util/BUILD]
//! ```
//!
//! Key shapes observed on Bazel 9.2.0, all of which this module only ever
//! splits on the first `:` and then reads structurally:
//!
//! - `PACKAGE:src/foo`, and `PACKAGE:@@repo+//` for a package outside the main
//!   repository.
//! - `FILE_STATE:[/abs/root]/[rel/path]`, where the root is the workspace for a
//!   path in the subject and `<absolute root>` for one outside it.
//! - `DIRECTORY_LISTING:[/abs/root]/[rel/path]`, same bracket pair.
//! - `BZL_LOAD:KeyForBuild{label=@@repo+//pkg:file.bzl, isBuildPrelude=false}`,
//!   and `KeyForBuiltins{...}` for the builtins bzl files.
//! - `GLOBS:<GlobsKey packageRoot = /abs, packageIdentifier = pkg, globRequests
//!   = [GlobRequest: ** FILES]>`.
//! - `CONFIGURED_TARGET:ConfiguredTargetKey{label=//pkg:name,
//!   config=BuildConfigurationKey[<hash>]}`. Source files get one of these too,
//!   which is what makes an analysis-tier dump a target graph and a
//!   loading-only one not.
//!
//! Nothing here interprets a key beyond its kind and the label or path inside
//! it, because the interesting question is which node *changed*, and that is
//! answered by comparing the key string and the dependency set verbatim.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context as _, Result};

/// A node's kind: everything before the first `:` of its key.
pub fn kind(key: &str) -> &str {
    key.split_once(':').map(|(k, _)| k).unwrap_or(key)
}

/// The rest of the key, after the first `:`.
pub fn body(key: &str) -> &str {
    key.split_once(':').map(|(_, v)| v).unwrap_or("")
}

/// One parsed dump: every node key, and the set of keys it depends on.
///
/// The dependency *groups* are flattened. Skyframe's groups are an evaluation
/// detail (which requests were made in one batch), and a change to the grouping
/// without a change to the set is not a change this oracle should react to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dump {
    pub nodes: BTreeMap<String, BTreeSet<String>>,
}

impl Dump {
    pub fn parse(text: &str) -> Self {
        let mut nodes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut current: Option<String> = None;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let indent = line.len() - line.trim_start().len();
            let trimmed = line.trim_end();
            match indent {
                0 => {
                    let key = trimmed.to_string();
                    nodes.entry(key.clone()).or_default();
                    current = Some(key);
                }
                // `  Group N:` is structure, not a dependency.
                2 => {}
                _ => {
                    if let Some(node) = &current {
                        let dep = trimmed.trim_start().to_string();
                        if let Some(set) = nodes.get_mut(node) {
                            set.insert(dep);
                        }
                    }
                }
            }
        }
        Self { nodes }
    }

    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the dump at {}", path.display()))?;
        Ok(Self::parse(&text))
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// How many nodes of each kind, for the result file's notes.
    pub fn counts(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for key in self.nodes.keys() {
            *counts.entry(kind(key).to_string()).or_insert(0) += 1;
        }
        counts
    }

    /// The reverse graph: for each key, the keys that depend on it.
    ///
    /// Dependencies naming a node the dump does not itself hold (the
    /// `--skykey_filter` drops most kinds) still get an entry, so a walk that
    /// starts at one of them is not silently empty.
    pub fn rdeps(&self) -> BTreeMap<&str, BTreeSet<&str>> {
        let mut rev: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for (node, deps) in &self.nodes {
            for dep in deps {
                rev.entry(dep.as_str()).or_default().insert(node.as_str());
            }
        }
        rev
    }

    /// Every key reachable by walking the reverse graph from `seeds`, the seeds
    /// included when the dump holds them.
    pub fn reverse_closure(&self, seeds: &BTreeSet<String>) -> BTreeSet<&str> {
        let rev = self.rdeps();
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut stack: Vec<&str> = Vec::new();
        for seed in seeds {
            // Borrow from the dump rather than from `seeds`, so what comes back
            // outlives the call; a seed the dump holds no node for is still a
            // valid starting point through the reverse index.
            let found = self
                .nodes
                .get_key_value(seed.as_str())
                .map(|(k, _)| k.as_str())
                .or_else(|| rev.get_key_value(seed.as_str()).map(|(k, _)| *k));
            if let Some(k) = found {
                if seen.insert(k) {
                    stack.push(k);
                }
            }
        }
        while let Some(node) = stack.pop() {
            let Some(parents) = rev.get(node) else {
                continue;
            };
            for p in parents {
                if seen.insert(p) {
                    stack.push(p);
                }
            }
        }
        seen
    }
}

/// How one node differs between two dumps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    /// In the head dump and not the parent's.
    Added,
    /// In the parent dump and not the head's.
    Removed,
    /// In both, with a different dependency set.
    DepsChanged,
}

impl Change {
    pub fn name(self) -> &'static str {
        match self {
            Change::Added => "added",
            Change::Removed => "removed",
            Change::DepsChanged => "deps-changed",
        }
    }
}

/// Every node that is not identical in both dumps.
pub fn diff(parent: &Dump, head: &Dump) -> BTreeMap<String, Change> {
    let mut out = BTreeMap::new();
    for (key, deps) in &head.nodes {
        match parent.nodes.get(key) {
            None => {
                out.insert(key.clone(), Change::Added);
            }
            Some(before) if before != deps => {
                out.insert(key.clone(), Change::DepsChanged);
            }
            Some(_) => {}
        }
    }
    for key in parent.nodes.keys() {
        if !head.nodes.contains_key(key) {
            out.insert(key.clone(), Change::Removed);
        }
    }
    out
}

/// The package a `PACKAGE:` key names, or `None` for a package outside the
/// main repository.
///
/// `PACKAGE:src/foo` is `src/foo`; the root package is `PACKAGE:` with an empty
/// body. `PACKAGE:@@rules_go+//go` belongs to another repository and no label
/// this oracle answers with can live there.
pub fn main_repo_package(key: &str) -> Option<&str> {
    if kind(key) != "PACKAGE" {
        return None;
    }
    let body = body(key);
    if body.starts_with('@') || body.contains("//") {
        return None;
    }
    Some(body)
}

/// The workspace-relative path a `FILE_STATE:` or `DIRECTORY_LISTING:` key
/// names, when its root is `root`.
///
/// The key is `KIND:[<root>]/[<rel>]`. A key whose root is not the workspace
/// (`<absolute root>`, or another repository's directory) is not a path in the
/// subject and gets `None`.
pub fn path_under(key: &str, root: &Path) -> Option<String> {
    let body = body(key);
    let rest = body.strip_prefix('[')?;
    let (dump_root, rest) = rest.split_once("]/[")?;
    let rel = rest.strip_suffix(']')?;
    if Path::new(dump_root) != root {
        return None;
    }
    Some(rel.to_string())
}

/// The `FILE_STATE:` key for a workspace-relative path.
pub fn file_state_key(root: &Path, rel: &str) -> String {
    format!("FILE_STATE:[{}]/[{rel}]", root.display())
}

/// The label a `CONFIGURED_TARGET:` key names, without its configuration.
///
/// `ConfiguredTargetKey{label=//pkg:name, config=BuildConfigurationKey[..]}`
/// becomes `//pkg:name`. Several configurations of one target collapse to the
/// same label, which is what the oracle compares against.
pub fn configured_target_label(key: &str) -> Option<&str> {
    if kind(key) != "CONFIGURED_TARGET" {
        return None;
    }
    let body = body(key);
    let at = body.find("label=")? + "label=".len();
    let rest = &body[at..];
    let end = rest
        .find(", config=")
        .or_else(|| rest.find('}'))
        .unwrap_or(rest.len());
    Some(rest[..end].trim())
}

/// The package part of a main-repository label, `//pkg:name` -> `pkg`.
///
/// `None` for a label outside `//`, which the oracle never answers with.
pub fn label_package(label: &str) -> Option<&str> {
    let rest = label.strip_prefix("//")?;
    Some(rest.split_once(':').map(|(p, _)| p).unwrap_or(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A verbatim slice of `bazel dump --skyframe=deps` on the `bazel` subject
    /// under Bazel 9.2.0, with the workspace root shortened to `/w`.
    const LOADING: &str = "\
PACKAGE:src/tools/launcher/util
  Group 1:
    PRECOMPUTED:lazy_macro_expansion_packages

  Group 2:
    PACKAGE_LOOKUP:src/tools/launcher/util

  Group 6:
    FILE:[/w]/[src/tools/launcher/util/BUILD]

  Group 11:
    BZL_LOAD:KeyForBuild{label=//tools/build_rules:prelude_bazel, isBuildPrelude=true}

  Group 13:
    GLOBS:<GlobsKey packageRoot = /w, packageIdentifier = src/tools/launcher/util, globRequests = [GlobRequest: ** FILES]>

FILE_STATE:[/w]/[src/tools/launcher/util/BUILD]

DIRECTORY_LISTING:[/w]/[src/tools/launcher/util]
  Group 1:
    FILE_STATE:[/w]/[src/tools/launcher/util]

BZL_LOAD:KeyForBuild{label=//tools/build_rules:prelude_bazel, isBuildPrelude=true}
  Group 1:
    FILE:[/w]/[tools/build_rules/prelude_bazel]
";

    /// The analysis tier, where a source file is a configured target of its own
    /// and the edges between configured targets are the target graph.
    const ANALYSIS: &str = "\
CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main/cpp:blaze_util, config=BuildConfigurationKey[abc123]}
  Group 1:
    PACKAGE:src/main/cpp

  Group 6:
    CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main/cpp:blaze_util.cc, config=BuildConfigurationKey[abc123]}

    CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main/cpp/util:errors, config=BuildConfigurationKey[abc123]}

CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main/cpp:blaze_util.cc, config=BuildConfigurationKey[abc123]}
  Group 1:
    PACKAGE:src/main/cpp

CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main/cpp:client, config=BuildConfigurationKey[abc123]}
  Group 6:
    CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main/cpp:blaze_util, config=BuildConfigurationKey[abc123]}
";

    #[test]
    fn a_node_owns_every_dependency_under_it_whatever_group_it_was_in() {
        let d = Dump::parse(LOADING);
        assert_eq!(d.len(), 4, "{:?}", d.nodes.keys().collect::<Vec<_>>());
        let pkg = &d.nodes["PACKAGE:src/tools/launcher/util"];
        // Five groups, five dependencies, the group headers themselves gone.
        assert_eq!(pkg.len(), 5, "{pkg:?}");
        assert!(pkg.contains("FILE:[/w]/[src/tools/launcher/util/BUILD]"));
        assert!(pkg.iter().all(|d| !d.starts_with("Group")));
    }

    #[test]
    fn a_node_with_no_dependencies_is_still_a_node() {
        let d = Dump::parse(LOADING);
        let key = "FILE_STATE:[/w]/[src/tools/launcher/util/BUILD]";
        assert!(d.nodes.contains_key(key));
        assert!(d.nodes[key].is_empty());
    }

    #[test]
    fn kinds_are_counted_by_the_prefix_before_the_first_colon() {
        let counts = Dump::parse(LOADING).counts();
        assert_eq!(counts["PACKAGE"], 1);
        assert_eq!(counts["FILE_STATE"], 1);
        assert_eq!(counts["DIRECTORY_LISTING"], 1);
        assert_eq!(counts["BZL_LOAD"], 1);
        // A key holding a colon inside its body is still one kind.
        assert!(!counts.contains_key("KeyForBuild{label=//tools/build_rules"));
    }

    #[test]
    fn a_diff_names_what_was_added_removed_and_rewired() {
        let parent = Dump::parse(LOADING);
        let mut head = parent.clone();
        head.nodes
            .remove("DIRECTORY_LISTING:[/w]/[src/tools/launcher/util]");
        head.nodes.insert("PACKAGE:src/new".into(), BTreeSet::new());
        head.nodes
            .get_mut("PACKAGE:src/tools/launcher/util")
            .unwrap()
            .insert("FILE:[/w]/[src/tools/launcher/util/extra.bzl]".into());

        let d = diff(&parent, &head);
        assert_eq!(d["PACKAGE:src/new"], Change::Added);
        assert_eq!(
            d["DIRECTORY_LISTING:[/w]/[src/tools/launcher/util]"],
            Change::Removed
        );
        assert_eq!(d["PACKAGE:src/tools/launcher/util"], Change::DepsChanged);
        assert_eq!(d.len(), 3, "{d:?}");
    }

    #[test]
    fn an_identical_dump_diffs_to_nothing() {
        let d = Dump::parse(LOADING);
        assert!(diff(&d, &d).is_empty());
    }

    #[test]
    fn the_reverse_closure_walks_from_a_file_state_to_its_package() {
        let d = Dump::parse(LOADING);
        let seeds: BTreeSet<String> = [file_state_key(Path::new("/w"), "src/tools/launcher/util")]
            .into_iter()
            .collect();
        let reached = d.reverse_closure(&seeds);
        // FILE_STATE -> DIRECTORY_LISTING, and no further in this slice.
        assert!(reached.contains("DIRECTORY_LISTING:[/w]/[src/tools/launcher/util]"));
        assert!(!reached.contains("PACKAGE:src/tools/launcher/util"));
    }

    #[test]
    fn a_seed_the_dump_never_names_reaches_nothing_rather_than_failing() {
        let d = Dump::parse(LOADING);
        let seeds: BTreeSet<String> = ["FILE_STATE:[/w]/[nowhere]".to_string()]
            .into_iter()
            .collect();
        assert!(d.reverse_closure(&seeds).is_empty());
    }

    #[test]
    fn the_analysis_tier_reverse_closure_is_the_target_graph() {
        let d = Dump::parse(ANALYSIS);
        let seed = "CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main/cpp:blaze_util.cc, \
                    config=BuildConfigurationKey[abc123]}";
        let seeds: BTreeSet<String> = [seed.to_string()].into_iter().collect();
        let labels: BTreeSet<&str> = d
            .reverse_closure(&seeds)
            .into_iter()
            .filter_map(configured_target_label)
            .collect();
        // The source file, the library that reads it, and the binary above it.
        assert!(labels.contains("//src/main/cpp:blaze_util"));
        assert!(labels.contains("//src/main/cpp:client"));
        assert!(labels.contains("//src/main/cpp:blaze_util.cc"));
        assert!(!labels.contains("//src/main/cpp/util:errors"));
    }

    #[test]
    fn a_configured_target_key_gives_up_its_label_without_its_configuration() {
        let key = "CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main/cpp:blaze_util, \
                   config=BuildConfigurationKey[abc123]}";
        assert_eq!(
            configured_target_label(key),
            Some("//src/main/cpp:blaze_util")
        );
        // An external label survives intact, for the caller to reject.
        let ext = "CONFIGURED_TARGET:ConfiguredTargetKey{label=@@rules_go+//go:toolchain, \
                   config=BuildConfigurationKey[d]}";
        assert_eq!(
            configured_target_label(ext),
            Some("@@rules_go+//go:toolchain")
        );
        // A key with no configuration at all still parses.
        let bare = "CONFIGURED_TARGET:ConfiguredTargetKey{label=//a:b}";
        assert_eq!(configured_target_label(bare), Some("//a:b"));
        assert_eq!(configured_target_label("PACKAGE:src/a"), None);
    }

    #[test]
    fn only_main_repository_packages_are_named() {
        assert_eq!(main_repo_package("PACKAGE:src/foo"), Some("src/foo"));
        assert_eq!(main_repo_package("PACKAGE:"), Some(""));
        assert_eq!(main_repo_package("PACKAGE:@@rules_go+//go"), None);
        assert_eq!(main_repo_package("FILE_STATE:[/w]/[a]"), None);
    }

    #[test]
    fn a_bracketed_key_gives_up_its_path_only_under_its_own_root() {
        let key = "FILE_STATE:[/w]/[src/a/BUILD]";
        assert_eq!(path_under(key, Path::new("/w")), Some("src/a/BUILD".into()));
        assert_eq!(path_under(key, Path::new("/other")), None);
        // The root Bazel prints for a path outside any workspace.
        let outside = "FILE_STATE:[<absolute root>]/[/some/cache/BUILD.tpl]";
        assert_eq!(path_under(outside, Path::new("/w")), None);
        // A directory listing uses the same bracket pair.
        assert_eq!(
            path_under("DIRECTORY_LISTING:[/w]/[src/a]", Path::new("/w")),
            Some("src/a".into())
        );
        // Round trip.
        assert_eq!(
            path_under(
                &file_state_key(Path::new("/w"), "src/a/BUILD"),
                Path::new("/w")
            ),
            Some("src/a/BUILD".into())
        );
    }

    #[test]
    fn a_label_says_which_package_it_is_in() {
        assert_eq!(label_package("//src/main/cpp:blaze"), Some("src/main/cpp"));
        assert_eq!(label_package("//:BUILD"), Some(""));
        assert_eq!(label_package("//src/main/cpp"), Some("src/main/cpp"));
        assert_eq!(label_package("@@rules_go+//go:toolchain"), None);
    }
}
