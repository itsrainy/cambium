//! The stored graph: what `prepare` leaves behind and `answer` walks.
//!
//! A parsed dump is reversed and written to one zstd-compressed file. The
//! format is text on purpose: it is read once per step, the compressor takes
//! the repetition out of it, and a format a person can read is worth more on
//! a prototype branch than the last few percent of size.
//!
//! ```text
//! cambium-skyframe-analysis 1
//! <node count>
//! <kind>\t<id>\t<config or empty>      one line per node, in index order
//! --
//! <rdep index>,<rdep index>,…          one line per node, in index order
//! ```

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use anyhow::{Context as _, Result};

use super::dump::{Dump, NodeKey, NodeKind};

pub const MAGIC: &str = "cambium-skyframe-analysis 1";

/// The reverse graph, as `answer` holds it.
#[derive(Debug, Default)]
pub struct Graph {
    pub nodes: Vec<NodeKey>,
    pub rdeps: Vec<Vec<u32>>,
    index: BTreeMap<String, u32>,
}

impl Graph {
    pub fn from_dump(dump: Dump) -> Self {
        let Dump {
            nodes,
            rdeps,
            index,
            ..
        } = dump;
        Self {
            nodes,
            rdeps,
            index,
        }
    }

    pub fn get(&self, key: &NodeKey) -> Option<u32> {
        self.index.get(&key.text()).copied()
    }

    /// Lookup by the stored key text, which is what a dump diff has in hand.
    pub fn contains_text(&self, text: &str) -> bool {
        self.index.contains_key(text)
    }

    /// Every configured label and the checksums it was analysed under. The
    /// answer to "can a `CONFIGURED_TARGET` key be normalised to a stable
    /// form": only if a label's checksum set is small and does not move.
    pub fn configs_by_label(&self) -> BTreeMap<&str, BTreeSet<&str>> {
        let mut out: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for n in &self.nodes {
            if n.kind == NodeKind::ConfiguredTarget {
                if let Some(c) = &n.config {
                    out.entry(n.id.as_str()).or_default().insert(c.as_str());
                }
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Every main-repository package the dump names, longest first, which is
    /// the order a path has to be matched in: `src/main/java` wins over
    /// `src` for a file under both.
    pub fn main_repo_packages(&self) -> Vec<&str> {
        let mut pkgs: BTreeSet<&str> = BTreeSet::new();
        for n in &self.nodes {
            if n.kind == NodeKind::Package && !n.id.starts_with('@') {
                pkgs.insert(n.id.as_str());
            }
        }
        let mut v: Vec<&str> = pkgs.into_iter().collect();
        // Longest first, then alphabetical, so the order is the same on
        // every run and a tie between two packages of one length is settled.
        v.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
        v
    }

    /// The transitive reverse closure of `seeds`, seeds included.
    pub fn reach(&self, seeds: &[u32]) -> BTreeSet<u32> {
        let mut seen: Vec<bool> = vec![false; self.nodes.len()];
        let mut queue: VecDeque<u32> = VecDeque::new();
        for &s in seeds {
            if (s as usize) < seen.len() && !seen[s as usize] {
                seen[s as usize] = true;
                queue.push_back(s);
            }
        }
        let mut out = BTreeSet::new();
        while let Some(n) = queue.pop_front() {
            out.insert(n);
            for &r in &self.rdeps[n as usize] {
                if !seen[r as usize] {
                    seen[r as usize] = true;
                    queue.push_back(r);
                }
            }
        }
        out
    }

    /// The answer a reached set amounts to: the labels of the
    /// main-repository `CONFIGURED_TARGET` nodes in it that carry a
    /// configuration, with the configuration dropped.
    ///
    /// A `config=null` node is a source file target, which is not a rule and
    /// is not what the oracle counts.
    pub fn answer_labels(&self, reached: &BTreeSet<u32>) -> BTreeSet<String> {
        reached
            .iter()
            .map(|&i| &self.nodes[i as usize])
            .filter(|n| {
                n.kind == NodeKind::ConfiguredTarget
                    && n.config.is_some()
                    && n.is_main_repo_label()
            })
            .map(|n| n.id.clone())
            .collect()
    }

    pub fn write(&self, path: &Path) -> Result<u64> {
        let file = std::fs::File::create(path)
            .with_context(|| format!("creating {}", path.display()))?;
        let mut w = zstd::Encoder::new(file, 3)?.auto_finish();
        writeln!(w, "{MAGIC}")?;
        writeln!(w, "{}", self.nodes.len())?;
        for n in &self.nodes {
            writeln!(
                w,
                "{}\t{}\t{}",
                n.kind.name(),
                n.id,
                n.config.as_deref().unwrap_or("")
            )?;
        }
        writeln!(w, "--")?;
        for r in &self.rdeps {
            let line: Vec<String> = r.iter().map(|i| i.to_string()).collect();
            writeln!(w, "{}", line.join(","))?;
        }
        drop(w);
        Ok(std::fs::metadata(path)?.len())
    }

    pub fn read(path: &Path) -> Result<Self> {
        let file =
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut lines = BufReader::new(zstd::Decoder::new(file)?).lines();
        let magic = lines.next().transpose()?.unwrap_or_default();
        anyhow::ensure!(magic == MAGIC, "{} is not a stored graph", path.display());
        let count: usize = lines
            .next()
            .transpose()?
            .unwrap_or_default()
            .trim()
            .parse()
            .context("node count")?;
        let mut g = Graph {
            nodes: Vec::with_capacity(count),
            rdeps: Vec::with_capacity(count),
            index: BTreeMap::new(),
        };
        for _ in 0..count {
            let line = lines.next().transpose()?.context("short node table")?;
            let mut f = line.splitn(3, '\t');
            let kind = NodeKind::of(f.next().unwrap_or_default());
            let id = f.next().unwrap_or_default().to_string();
            let config = f.next().unwrap_or_default();
            let key = NodeKey {
                kind,
                id,
                config: (!config.is_empty()).then(|| config.to_string()),
            };
            g.index.insert(key.text(), g.nodes.len() as u32);
            g.nodes.push(key);
        }
        let sep = lines.next().transpose()?.unwrap_or_default();
        anyhow::ensure!(sep == "--", "missing edge separator in {}", path.display());
        for _ in 0..count {
            let line = lines.next().transpose()?.context("short edge table")?;
            let row: Vec<u32> = if line.is_empty() {
                Vec::new()
            } else {
                line.split(',').filter_map(|s| s.parse().ok()).collect()
            };
            g.rdeps.push(row);
        }
        Ok(g)
    }
}

/// Which nodes a change set names.
///
/// Four seeding routes, because the analysis graph reaches a source file in
/// four different ways and a change set does not say which one applies:
///
/// 1. The **source file target**: `CONFIGURED_TARGET|//pkg:rest` with no
///    configuration. This is the only route that carries a *content* edit of
///    an ordinary source file, because no analysis-phase node reads the
///    bytes of a `.java` or a `.cc`; what depends on the file is the rule
///    whose `srcs` name it.
/// 2. The **file node**: `FILE` and `FILE_STATE` under the workspace root.
///    This is the route a `BUILD` or `.bzl` edit travels
///    (`FILE_STATE -> FILE -> PACKAGE -> CONFIGURED_TARGET`).
/// 3. The **load key**: `BZL_LOAD` for a `.bzl` path, in case the file node
///    is absent because the dump was filtered before the `.bzl` was read.
/// 4. The **directory listing** of the containing directory, for an added or
///    deleted path, which is what a `glob()` re-evaluates over.
///
/// `packages` is `main_repo_packages`, longest first.
pub fn seeds(
    graph: &Graph,
    workspace: &str,
    changed: &[crate::skyframe::ChangedPath],
    packages: &[&str],
) -> (Vec<u32>, BTreeMap<String, usize>) {
    let mut out: BTreeSet<u32> = BTreeSet::new();
    let mut by_route: BTreeMap<String, usize> = BTreeMap::new();
    let hit = |out: &mut BTreeSet<u32>,
                   by_route: &mut BTreeMap<String, usize>,
                   route: &str,
                   key: NodeKey| {
        if let Some(i) = graph.get(&key) {
            if out.insert(i) {
                *by_route.entry(route.to_string()).or_default() += 1;
            }
        }
    };
    for c in changed {
        let path = c.path.as_str();
        if let Some(label) = source_label(path, packages) {
            hit(
                &mut out,
                &mut by_route,
                "source_target",
                NodeKey::configured_target(&label, None),
            );
            if path.ends_with(".bzl") || path.ends_with("prelude_bazel") {
                hit(
                    &mut out,
                    &mut by_route,
                    "bzl_load",
                    NodeKey::simple(NodeKind::BzlLoad, label.clone()),
                );
            }
        }
        let rooted = format!("{workspace}\u{1}{path}");
        hit(
            &mut out,
            &mut by_route,
            "file",
            NodeKey::simple(NodeKind::File, rooted.clone()),
        );
        hit(
            &mut out,
            &mut by_route,
            "file_state",
            NodeKey::simple(NodeKind::FileState, rooted),
        );
        let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let rooted_dir = format!("{workspace}\u{1}{dir}");
        hit(
            &mut out,
            &mut by_route,
            "directory_listing",
            NodeKey::simple(NodeKind::DirectoryListing, rooted_dir.clone()),
        );
        hit(
            &mut out,
            &mut by_route,
            "directory_listing_state",
            NodeKey::simple(NodeKind::DirectoryListingState, rooted_dir),
        );
        // A BUILD file is the package's own definition; its package node is
        // the seed whether or not the file node survived the filter.
        if is_build_file(path) {
            hit(
                &mut out,
                &mut by_route,
                "package",
                NodeKey::simple(NodeKind::Package, dir.to_string()),
            );
        }
    }
    (out.into_iter().collect(), by_route)
}

pub fn is_build_file(path: &str) -> bool {
    let name = path.rsplit_once('/').map(|(_, n)| n).unwrap_or(path);
    name == "BUILD" || name == "BUILD.bazel"
}

/// `src/main/java/Foo.java` -> `//src/main:java/Foo.java`, given the package
/// list longest first. `None` when no package contains the path, which is
/// what a file outside every package (a `.bazelrc`, a README at a root with
/// no `BUILD`) gets.
pub fn source_label(path: &str, packages: &[&str]) -> Option<String> {
    for p in packages {
        if p.is_empty() {
            if !path.contains('/') {
                return Some(format!("//:{path}"));
            }
            continue;
        }
        if let Some(rest) = path.strip_prefix(*p) {
            if let Some(rest) = rest.strip_prefix('/') {
                return Some(format!("//{p}:{rest}"));
            }
        }
    }
    // The root package, if the dump named it, is the fallback for anything
    // else; `packages` holds it as the empty string.
    packages
        .contains(&"")
        .then(|| format!("//:{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skyframe::ChangedPath;

    fn fixture() -> Graph {
        let text = "\
CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main:foo, config=BuildConfigurationKey[c1]}
  Group 1:
    CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main:Foo.java, config=null}

    PACKAGE:src/main

CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/other:bar, config=BuildConfigurationKey[c1]}
  Group 1:
    CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main:foo, config=BuildConfigurationKey[c1]}

CONFIGURED_TARGET:ConfiguredTargetKey{label=@@ext+//:x, config=BuildConfigurationKey[c1]}
  Group 1:
    CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main:foo, config=BuildConfigurationKey[c1]}

PACKAGE:src/main
  Group 1:
    FILE:[/w]/[src/main/BUILD]

PACKAGE:src/other
  Group 1:
    FILE:[/w]/[src/other/BUILD]

FILE:[/w]/[src/main/BUILD]
  Group 1:
    FILE_STATE:[/w]/[src/main/BUILD]
";
        Graph::from_dump(super::super::dump::parse(std::io::Cursor::new(text)).unwrap())
    }

    fn changed(paths: &[(&str, &str)]) -> Vec<ChangedPath> {
        paths
            .iter()
            .map(|(s, p)| ChangedPath {
                status: s.to_string(),
                path: p.to_string(),
            })
            .collect()
    }

    #[test]
    fn a_path_takes_the_longest_package_that_contains_it() {
        let pkgs = ["src/main/java", "src/main", "src", ""];
        assert_eq!(
            source_label("src/main/java/Foo.java", &pkgs).unwrap(),
            "//src/main/java:Foo.java"
        );
        assert_eq!(
            source_label("src/main/Bar.java", &pkgs).unwrap(),
            "//src/main:Bar.java"
        );
        // Nothing under a package, so the root package takes it.
        assert_eq!(source_label("README.md", &pkgs).unwrap(), "//:README.md");
        // And with no root package in the dump, a path outside every package
        // has no source label at all.
        assert_eq!(source_label("README.md", &["src/main"]), None);
    }

    #[test]
    fn a_content_edit_seeds_the_source_file_target() {
        let g = fixture();
        let pkgs = g.main_repo_packages();
        let (seeds, routes) = seeds(&g, "/w", &changed(&[("M", "src/main/Foo.java")]), &pkgs);
        assert_eq!(routes.get("source_target"), Some(&1));
        // No FILE node for a .java: analysis never reads its bytes, and the
        // rule that names it in srcs is the only route.
        assert!(!routes.contains_key("file"));
        let labels = g.answer_labels(&g.reach(&seeds));
        assert!(labels.contains("//src/main:foo"), "{labels:?}");
        assert!(labels.contains("//src/other:bar"), "{labels:?}");
        // The external dependent is not a main-repository rule.
        assert!(!labels.contains("@@ext+//:x"), "{labels:?}");
    }

    #[test]
    fn a_build_edit_seeds_the_file_the_package_and_the_file_state() {
        let g = fixture();
        let pkgs = g.main_repo_packages();
        let (seeds, routes) = seeds(&g, "/w", &changed(&[("M", "src/main/BUILD")]), &pkgs);
        assert_eq!(routes.get("file"), Some(&1));
        assert_eq!(routes.get("file_state"), Some(&1));
        assert_eq!(routes.get("package"), Some(&1));
        let labels = g.answer_labels(&g.reach(&seeds));
        assert!(labels.contains("//src/main:foo"), "{labels:?}");
        assert!(labels.contains("//src/other:bar"), "{labels:?}");
    }

    #[test]
    fn a_path_the_dump_never_saw_seeds_nothing() {
        let g = fixture();
        let pkgs = g.main_repo_packages();
        let (seeds, _) = seeds(&g, "/w", &changed(&[("M", "docs/README.md")]), &pkgs);
        assert!(seeds.is_empty());
        assert!(g.answer_labels(&g.reach(&seeds)).is_empty());
    }

    #[test]
    fn packages_come_back_longest_first() {
        let g = fixture();
        let pkgs = g.main_repo_packages();
        // `src/other` is the longer of the two, so it is matched first.
        assert_eq!(pkgs, vec!["src/other", "src/main"]);
    }

    #[test]
    fn a_graph_round_trips_through_its_file() {
        let g = fixture();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.zst");
        let bytes = g.write(&path).unwrap();
        assert!(bytes > 0);
        let back = Graph::read(&path).unwrap();
        assert_eq!(back.len(), g.len());
        assert_eq!(back.nodes, g.nodes);
        assert_eq!(back.rdeps, g.rdeps);
        let pkgs = back.main_repo_packages();
        let (seeds, _) = seeds(&back, "/w", &changed(&[("M", "src/main/Foo.java")]), &pkgs);
        assert!(back
            .answer_labels(&back.reach(&seeds))
            .contains("//src/main:foo"));
    }

    #[test]
    fn a_file_that_is_not_a_stored_graph_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk.zst");
        let f = std::fs::File::create(&path).unwrap();
        let mut w = zstd::Encoder::new(f, 1).unwrap().auto_finish();
        writeln!(w, "something else").unwrap();
        drop(w);
        assert!(Graph::read(&path).is_err());
    }
}
