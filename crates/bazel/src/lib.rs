//! Spawning Bazel.
//!
//! Everything here is deliberately thin: two query shapes (a package set and
//! the whole repo), one `rbuildfiles` call, and version detection. The flags
//! are pinned to what the spec's core invariant needs — unconfigured graph,
//! unflattened selects, implicit deps left in — and are the only Bazel surface
//! cambium depends on.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;

pub mod convert;

pub use convert::{convert, ConvertOptions, ParsedTarget};

pub use cambium_index::externals::LeafReach;
use cambium_proto::reader::{read_frames, ReadError};

#[derive(Debug, thiserror::Error)]
pub enum BazelError {
    #[error(
        "no Bazel workspace found: walked up from {0} without finding MODULE.bazel or WORKSPACE"
    )]
    NoWorkspace(PathBuf),
    #[error("failed to run {binary}: {source}")]
    Spawn {
        binary: String,
        #[source]
        source: std::io::Error,
    },
    #[error("bazel {args} exited with {status}\n--- last stderr ---\n{stderr}")]
    Failed {
        args: String,
        status: String,
        stderr: String,
    },
    #[error("reading bazel output: {0}")]
    Read(#[from] ReadError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

type Result<T> = std::result::Result<T, BazelError>;

/// How many trailing stderr lines to keep for error reporting (spec 7.4).
const STDERR_TAIL_LINES: usize = 40;

/// Query expressions longer than this go through `--query_file` instead of argv.
const MAX_INLINE_QUERY: usize = 50_000;

/// Flags every invocation gets (spec 7.4).
const HYGIENE: &[&str] = &[
    "--noshow_progress",
    "--noshow_loading_progress",
    "--ui_event_filters=-info",
];

/// Flags that define the graph we index (spec 7.1).
///
/// Note `--proto:flatten_selects=false`, where the spec calls for `true`.
/// (It has to be passed explicitly: Bazel defaults it to true.) Flattening is
/// not needed for the edges — `rule_input` already carries
/// the union over every `select` branch, plus the `config_setting` labels the
/// branches key on — and it actively costs soundness: with it on, an attribute
/// is reported as that same union, so editing one branch to a value another
/// branch already names leaves the rule hash unchanged and the target is
/// silently reported as unaffected. Unflattened, the attribute keeps its
/// per-branch entries, the hash sees the edit, and the edges are identical.
/// (Verified against Bazel 7.4.1; covered by the `change select branch` case in
/// the end-to-end tests.)
///
/// Note also that `--noimplicit_deps` is *not* passed, where it used to be.
///
/// Implicit deps were dropped to keep the toolchain graph — which is external,
/// enormous and never changes — out of the index. It also dropped every
/// implicit edge that points *into the main repository*, and those are real
/// build inputs. sourcegraph's schema generator is the clean example:
///
/// ```text
/// generate_schemas = rule(attrs = {
///     "_sg": attr.label(executable = True, default = "//dev/sg:sg", cfg = "exec"),
/// })
/// ```
///
/// `somepath(//internal/database:generate_schemas, //dev/sg/internal/cloud:cloud)`
/// answers with a three-hop path, and the same query under `--noimplicit_deps`
/// answers "Empty results" — so a commit touching only `dev/sg/internal/cloud`
/// left the generated schemas, and the eleven targets downstream of them,
/// out of cambium's affected set. That is unsoundness, not
/// over-approximation, and no invalidator can name it.
///
/// The cost of keeping them turned out to be nearly nothing, measured on both
/// benchmark subject at its pin: the `//...` proto stream grows 3% on
/// sourcegraph (36.4 -> 37.5 MB) and 4% on envoy (53.5 -> 55.6 MB), the wall
/// time does not move, and neither repository reports a single extra load
/// error. The toolchain labels the flag was there to exclude arrive as
/// external `Unknown` nodes with no outgoing edges: a handful of leaves that
/// never appear in a diff.
const QUERY_FLAGS: &[&str] = &[
    "--output=streamed_proto",
    "--order_output=no",
    "--keep_going",
    "--proto:locations=false",
    "--proto:default_values=false",
    "--proto:flatten_selects=false",
];

/// Result of a query that ran to completion, successfully or not.
#[derive(Debug)]
pub struct QueryOutcome {
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stderr_tail: String,
    pub targets_read: usize,
    /// Packages Bazel reported as failing to load, counted over the *whole*
    /// stderr stream rather than the 40-line tail.
    pub load_errors: LoadErrors,
    /// Packages Bazel said were *declared* deleted by `--deleted_packages`.
    /// Not load failures: see [`parse_deleted_package_line`].
    pub deleted_packages: Vec<String>,
}

/// What `--keep_going` could not load, scraped from stderr.
///
/// Bazel does not report this in the proto stream — a package that failed to
/// load simply is not there — so the only machine-readable trace of an
/// incomplete answer is the `ERROR:` lines. `count` is every such line;
/// `packages` keeps the first [`MAX_ERROR_PACKAGES`] names, which is enough to
/// tell "one broken package" from "the whole repository failed to resolve its
/// external deps" when reading an index's provenance months later.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadErrors {
    pub count: u32,
    pub packages: Vec<String>,
}

impl LoadErrors {
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn push(&mut self, package: &str) {
        self.count += 1;
        if self.packages.len() < MAX_ERROR_PACKAGES {
            self.packages.push(package.to_string());
        }
    }
}

/// How many failing package names an outcome keeps.
pub const MAX_ERROR_PACKAGES: usize = 32;

/// The repository token of a label as Bazel printed it, without its `@`s:
/// `grpc+` for `@@grpc+//src/core:x`, `grpc` for `@grpc//:grpc`. `None` for
/// a main-repository label.
pub fn label_repo_token(label: &str) -> Option<&str> {
    let rest = label.strip_prefix('@')?;
    let rest = rest.strip_prefix('@').unwrap_or(rest);
    let (repo, _) = rest.split_once("//")?;
    (!repo.is_empty()).then_some(repo)
}

/// The apparent name a canonical repository token most likely carries, for a
/// token read off a label rather than the repository mapping: `grpc+` is
/// `grpc` (a `bazel_dep`), `+ext+name` is `name` (an extension-generated
/// repository), and a token with no `+` is already apparent (WORKSPACE
/// mode). The walk matches either spelling against the index's names
/// ([`crate::repo_token_matches`]), so a guess that is wrong costs a
/// consumer that never dirties, never a wrong one.
pub fn apparent_of_canonical(token: &str) -> String {
    match token.rsplit_once('+') {
        Some((_, tail)) if !tail.is_empty() => tail.to_string(),
        Some((head, _)) => head.rsplit('+').next().unwrap_or(head).to_string(),
        None => token.to_string(),
    }
}

/// The package part of a `@R//pkg` line, if `R` is the repository `repo`
/// names, in either spelling Bazel prints.
///
/// Under bzlmod `bazel query` prints the *canonical* name, which is the
/// apparent name with the extension that made it and a separator around it:
/// `@@tsl+//tsl/platform` for `@tsl`, `@@+xla_extension+tsl//...` where an
/// extension generated it. Reading only the apparent `@tsl//` spelling drops
/// every line on Bazel 7 and later, which reads as a repository with no
/// packages at all.
fn package_of_repo(line: &str, repo: &str) -> Option<String> {
    let (name, pkg) = line.trim_start_matches('@').split_once("//")?;
    let named = token_names(name, repo);
    // The package, not the label: `@r//a/b:pkg` is the package `a/b`, which is
    // the spelling the consumer table is matched against at run time.
    named.then(|| {
        let pkg = pkg.split_once(':').map_or(pkg, |(p, _)| p);
        pkg.trim_start_matches('/').to_string()
    })
}

/// Whether a repository token as Bazel printed it (`grpc`, `grpc+`,
/// `+xla_extension+tsl`, Bazel 7's `grpc~`) names the repository `repo`
/// goes by: the token itself, or the last segment of its canonical form.
fn token_names(token: &str, repo: &str) -> bool {
    token == repo
        || token
            .trim_end_matches(['+', '~'])
            .rsplit(['+', '~'])
            .next()
            .is_some_and(|last| last == repo)
}

/// `set("a" "b" ...)`, each label quoted: `@grpc//:grpc++` is a syntax
/// error bare, since `+` is the union operator.
fn quoted_set(labels: &[String]) -> String {
    let inner = labels
        .iter()
        .map(|l| format!("\"{l}\""))
        .collect::<Vec<_>>()
        .join(" ");
    format!("set({inner})")
}

/// The consumer table's second query: what the bridge rules depend on inside
/// `@repo`, transitively.
///
/// It was `deps(set(bridge), 1)` once, which records the packages a bridge
/// rule names and not the ones those name in turn: 36 packages of
/// `@envoy_api` at envoy's pin against a transitive closure of 48, the
/// twelve missing including `envoy/type/metadata/v3` and
/// `envoy/annotations`, which `envoy/config/route/v3` depends on. An edit
/// confined to one of those skipped the consumer.
fn consumed_expr(repo: &str, bridge: &[String]) -> String {
    format!("deps({}) intersect @{repo}//...:*", quoted_set(bridge))
}

/// The query behind [`Bazel::leaf_reaches`]: the leaves' transitive deps,
/// less the source files outside `@repo`.
///
/// Every rule and generated file on the leaves' closure stays, so each leaf
/// can be walked forward on its own and what it passes through read off the
/// walk ([`LeafReach::via`]). A source file is an edge's end and never its
/// middle, so dropping one loses no path; `@repo`'s are kept because a leaf
/// that names one of its files directly reaches that package. No `kind()`
/// filter beyond that: a path through a generated file is a path.
fn leaf_expr(repo: &str, leaves: &[String]) -> String {
    format!(
        "let d = deps({}) in $d except (kind(\"source file\", $d) except @{repo}//...:*)",
        quoted_set(leaves)
    )
}

/// A label's identity up to how its repository is spelled: the apparent
/// name and what follows `//`. `@@grpc+//:grpc++`, `@@grpc~//:grpc++` (Bazel
/// 7's separator) and `@grpc//:grpc++` are one key; a main-repo label keeps
/// an empty name.
fn spelling_key(label: &str) -> Option<(&str, &str)> {
    let rest = label
        .strip_prefix('@')
        .map_or(label, |r| r.strip_prefix('@').unwrap_or(r));
    let (repo, target) = rest.split_once("//")?;
    let apparent = repo
        .trim_end_matches(['+', '~'])
        .rsplit(['+', '~'])
        .next()
        .unwrap_or(repo);
    Some((apparent, target))
}

/// What a node of [`Bazel::leaf_reaches`]'s graph that is not in `@repo`
/// contributes to [`LeafReach::via`]: `//pkg` for a main-repo node, `@name`
/// for any other repository's, the name as Bazel printed it.
fn via_of(label: &str) -> Option<String> {
    let main = label
        .strip_prefix("@@//")
        .or_else(|| label.strip_prefix("@//"))
        .or_else(|| label.strip_prefix("//"));
    match main {
        Some(rest) => Some(format!(
            "//{}",
            rest.split_once(':').map_or(rest, |(p, _)| p)
        )),
        None => label_repo_token(label).map(|t| format!("@{t}")),
    }
}

/// A graph as `bazel query --output=graph --nograph:factored` prints it,
/// read into adjacency lists: one node per line (`"a"`), one edge per line
/// (`"a" -> "b"`), and for a `select` edge a following `[label="..."]` line,
/// which carries no quoted node at its start and is skipped.
struct LabelGraph<'g> {
    names: Vec<&'g str>,
    edges: Vec<Vec<usize>>,
    by_key: BTreeMap<(&'g str, &'g str), Vec<usize>>,
}

impl<'g> LabelGraph<'g> {
    fn parse(graph: &'g str) -> Self {
        let mut ids: BTreeMap<&str, usize> = BTreeMap::new();
        let mut names: Vec<&str> = Vec::new();
        let mut edges: Vec<Vec<usize>> = Vec::new();
        for line in graph.lines() {
            let line = line.trim();
            let Some(body) = line.strip_prefix('"') else {
                continue;
            };
            let Some((from, rest)) = body.split_once('"') else {
                continue;
            };
            let from_id = *ids.entry(from).or_insert_with(|| {
                names.push(from);
                edges.push(Vec::new());
                names.len() - 1
            });
            let Some(to) = rest
                .trim_start()
                .strip_prefix("->")
                .map(str::trim_start)
                .and_then(|r| r.strip_prefix('"'))
                .and_then(|r| r.split_once('"'))
                .map(|(to, _)| to)
            else {
                continue;
            };
            let to_id = *ids.entry(to).or_insert_with(|| {
                names.push(to);
                edges.push(Vec::new());
                names.len() - 1
            });
            edges[from_id].push(to_id);
        }
        let mut by_key: BTreeMap<(&str, &str), Vec<usize>> = BTreeMap::new();
        for (i, n) in names.iter().enumerate() {
            if let Some(k) = spelling_key(n) {
                by_key.entry(k).or_default().push(i);
            }
        }
        Self {
            names,
            edges,
            by_key,
        }
    }

    /// Every node that is `label` up to how its repository is spelled
    /// ([`spelling_key`]); empty when the graph never printed it.
    fn starts<'a>(&'a self, label: &'a str) -> Vec<usize> {
        // Read as a map whose keys live as long as `label` does, which the
        // map's covariance allows.
        let by_key: &'a BTreeMap<(&'a str, &'a str), Vec<usize>> = &self.by_key;
        spelling_key(label)
            .and_then(|k| by_key.get(&k))
            .cloned()
            .unwrap_or_default()
    }
}

/// Reads [`Bazel::leaf_reaches`]'s graph: per leaf, the packages of `repo`
/// among the nodes reachable from it, and what outside `repo` they are
/// reached through.
///
/// The leaves are matched to the graph's nodes by [`spelling_key`], every
/// node that shares the key being a start, and the answer is keyed by the
/// caller's spelling; a leaf no node matches is left out.
fn parse_leaf_reach(graph: &str, repo: &str, leaves: &[String]) -> BTreeMap<String, LeafReach> {
    let g = LabelGraph::parse(graph);
    let (names, edges) = (&g.names, &g.edges);
    let mut out = BTreeMap::new();
    for leaf in leaves {
        let starts = g.starts(leaf);
        if starts.is_empty() {
            continue;
        }
        let mut seen = vec![false; names.len()];
        let mut stack = Vec::new();
        for &s in &starts {
            seen[s] = true;
            stack.push(s);
        }
        let mut reach = LeafReach::default();
        while let Some(n) = stack.pop() {
            match package_of_repo(names[n], repo) {
                Some(p) => {
                    reach.packages.insert(p);
                }
                None => {
                    if let Some(v) = via_of(names[n]) {
                        reach.via.insert(v);
                    }
                }
            }
            for &m in &edges[n] {
                if !seen[m] {
                    seen[m] = true;
                    stack.push(m);
                }
            }
        }
        out.insert(leaf.clone(), reach);
    }
    out
}

/// What a label of the round-trip graph is to the index: `Some` for a node
/// inside the index's universe, spelled the way the index spells it
/// (`//pkg:name` for the main repository, `@name//pkg:name` for an indexed
/// repository under the name the index knows it by), `None` for a node
/// outside it.
fn inside_label(label: &str, indexed: &[String]) -> Option<String> {
    let main = label
        .strip_prefix("@@//")
        .or_else(|| label.strip_prefix("@//"))
        .or_else(|| label.strip_prefix("//"));
    if let Some(rest) = main {
        return Some(format!("//{rest}"));
    }
    let token = label_repo_token(label)?;
    let (_, rest) = label.split_once("//")?;
    indexed
        .iter()
        .find(|r| token_names(token, r))
        .map(|r| format!("@{r}//{rest}"))
}

/// The package patterns of the universe a closure reached, from
/// `--output=package` lines: `//pkg:*` for a main-repository package (a line
/// with no repository, `""` being the root package), `@R//pkg:*` for one of
/// an indexed repository, spelled as Bazel printed it. Everything else is
/// outside and dropped.
fn inside_patterns(packages: &str, indexed: &[String]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in packages.lines().map(str::trim) {
        let main = line
            .strip_prefix("@@//")
            .or_else(|| line.strip_prefix("@//"))
            .or_else(|| (!line.starts_with('@')).then_some(line));
        if let Some(pkg) = main {
            out.insert(format!("//{pkg}:*"));
            continue;
        }
        let Some((repo, pkg)) = line.trim_start_matches('@').split_once("//") else {
            continue;
        };
        if indexed.iter().any(|r| token_names(repo, r)) {
            out.insert(format!("{}//{pkg}:*", &line[..line.len() - pkg.len() - 2]));
        }
    }
    out
}

/// The packages of a graph's nodes, one line each, in the spelling
/// `--output=package` prints and [`inside_patterns`] reads: `pkg` for the
/// main repository's, `@R//pkg` (as printed) for any other's.
fn package_lines(g: &LabelGraph<'_>) -> String {
    let mut lines: BTreeSet<&str> = BTreeSet::new();
    for n in &g.names {
        let pkg = n.split_once(':').map_or(*n, |(p, _)| p);
        lines.insert(pkg.strip_prefix("//").unwrap_or(pkg));
    }
    lines.into_iter().collect::<Vec<_>>().join("\n")
}

/// Reads the closure graph behind [`Bazel::round_trips`]: per leaf, the
/// repositories of the nodes outside the universe it reaches through nodes
/// outside it alone, each `@name` as printed ([`RoundTrips::closure`]).
///
/// The walk stops where [`parse_round_trips`] does, at the first node
/// inside. What lies beyond is the index's own rows, whose external deps are
/// leaves of their own: a repository the main repository reaches past an
/// entry is seeded through that leaf, and the reverse walk climbs to the
/// entry and across the round-trip edge back to this leaf. A leaf the graph
/// never printed is left out.
fn parse_closure_repos(
    g: &LabelGraph<'_>,
    indexed: &[String],
    leaves: &[String],
) -> BTreeMap<String, BTreeSet<String>> {
    let inside: Vec<bool> = g
        .names
        .iter()
        .map(|n| inside_label(n, indexed).is_some())
        .collect();
    let mut out = BTreeMap::new();
    for leaf in leaves {
        let starts = g.starts(leaf);
        if starts.is_empty() {
            continue;
        }
        let mut seen = vec![false; g.names.len()];
        let mut stack: Vec<usize> = Vec::new();
        for &s in &starts {
            seen[s] = true;
            stack.push(s);
        }
        let mut repos = BTreeSet::new();
        while let Some(n) = stack.pop() {
            if inside[n] && !starts.contains(&n) {
                continue;
            }
            if let Some(t) = label_repo_token(g.names[n]) {
                repos.insert(format!("@{t}"));
            }
            for &m in &g.edges[n] {
                if !seen[m] {
                    seen[m] = true;
                    stack.push(m);
                }
            }
        }
        out.insert(leaf.clone(), repos);
    }
    out
}

/// Bazel's answer to [`Bazel::round_trips`]: the edges, and what they were
/// derived through that a run can see move without a repository outside the
/// universe moving.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoundTrips {
    /// Leaf -> the labels inside the universe it reaches through labels
    /// outside it alone, spelled as the index spells them. A leaf that
    /// reaches nothing inside, or that Bazel could not resolve, is left out.
    pub reach: BTreeMap<String, BTreeSet<String>>,
    /// The files the packages outside the universe on the leaves' closure
    /// load that are not themselves outside it ([`closure_loads_via`]):
    /// `//pkg:file` for the main repository's, `@name` for an indexed
    /// repository's.
    pub via: BTreeSet<String>,
    /// Leaf -> the repositories its closure runs through outside the
    /// universe, its own included, each `@name` as Bazel printed the name
    /// (the spelling of [`RoundTrips::via`]). A repository on the closure
    /// that moves moves the leaf: Bazel refetches it and rebuilds everything
    /// above it, the leaf's rules included. On envoy `@grpc//:grpc++` runs
    /// through `@c-ares`, and the rules above `grpc++` reach `@c-ares` only
    /// that way. A leaf Bazel could not resolve is left out.
    pub closure: BTreeMap<String, BTreeSet<String>>,
}

/// The query behind [`Bazel::closure_loads`]: every `.bzl` (and `.scl`) file
/// loaded to load the packages of the leaves' closure, less the packages
/// `except` names.
fn loads_expr(leaves: &[String], except: &BTreeSet<String>) -> String {
    let d = format!("deps({})", quoted_set(leaves));
    if except.is_empty() {
        return format!("loadfiles({d})");
    }
    let except = except.iter().cloned().collect::<Vec<_>>().join(" + ");
    format!("loadfiles({d} except ({except}))")
}

/// Reads [`Bazel::closure_loads`]'s `--output=label` lines into via entries:
/// `//pkg:file` for a file of the main repository (the root package's
/// `//:file`), `@name` for one of an `indexed` repository, under the name
/// the index knows it by. Every other repository's files are dropped: a run
/// in which such a repository moves asks for the edges again whatever they
/// were derived through.
///
/// This is what makes an answer about a path through an external rule
/// depend on the main repository. On `bazelbuild/rules_shell` at `b1eed28`
/// `@bazel_tools//tools/test:collect_coverage` is an `sh_binary`, whose rule
/// class is the root module's own `//shell/private:sh_binary.bzl`, and its
/// edge to `//shell/runfiles:runfiles` is that class's implicit
/// `_runfiles_dep`: an edit to the `.bzl` moves the leaf's edges, and no
/// `BUILD` file or repository outside moved.
fn closure_loads_via(labels: &str, indexed: &[String]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in labels.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let main = line
            .strip_prefix("@@//")
            .or_else(|| line.strip_prefix("@//"))
            .or_else(|| line.strip_prefix("//"));
        if let Some(rest) = main {
            out.insert(format!("//{rest}"));
            continue;
        }
        if let Some(r) =
            label_repo_token(line).and_then(|t| indexed.iter().find(|r| token_names(t, r)))
        {
            out.insert(format!("@{r}"));
        }
    }
    out
}

/// Reads [`Bazel::round_trips`]'s graph: per leaf, the nodes inside the
/// universe it reaches through nodes outside it alone.
///
/// The walk from a leaf goes on through every outside node and stops at the
/// first inside one, which it records. What lies beyond that node is the
/// index's own rows, re-queried with its package, so a `BUILD` edit there
/// never makes an answer stale; a path that leaves again through another
/// leaf is that leaf's answer. What the outside part was derived through is
/// the other half of the answer ([`RoundTrips::via`]). A leaf that reaches
/// nothing, or that the graph never printed, is left out.
fn parse_round_trips(
    g: &LabelGraph<'_>,
    indexed: &[String],
    leaves: &[String],
) -> BTreeMap<String, BTreeSet<String>> {
    let inside: Vec<Option<String>> = g.names.iter().map(|n| inside_label(n, indexed)).collect();
    let mut out = BTreeMap::new();
    for leaf in leaves {
        let starts = g.starts(leaf);
        let mut seen = vec![false; g.names.len()];
        let mut stack: Vec<usize> = Vec::new();
        for &s in &starts {
            seen[s] = true;
            stack.push(s);
        }
        let mut entries = BTreeSet::new();
        while let Some(n) = stack.pop() {
            // A leaf is never its own entry, whatever its spelling says.
            if let Some(l) = inside[n].as_ref().filter(|_| !starts.contains(&n)) {
                entries.insert(l.clone());
                continue;
            }
            for &m in &g.edges[n] {
                if !seen[m] {
                    seen[m] = true;
                    stack.push(m);
                }
            }
        }
        if !entries.is_empty() {
            out.insert(leaf.clone(), entries);
        }
    }
    out
}

/// The package named by one Bazel load-error line, if it is one.
///
/// Four shapes, all emitted under `--keep_going`, and all interleaved with
/// the ANSI colour codes Bazel writes even when stderr is a pipe:
///
/// ```text
/// ERROR: error loading package 'modules/planning': at /...: ...
/// ERROR: package contains errors: third_party/bazel: name 'py_library' ...
/// ERROR: no such package '@@local_config_cuda//cuda': ...
/// ERROR: Skipping '//xla/mlir_hlo:*': error loading package 'xla/mlir_hlo': Encountered error while reading extension file 'mlir/tblgen.bzl': no such package '@@+llvm_extension+llvm-project//mlir': ...
/// ```
///
/// The first three are what a `//...` query prints. A query of named
/// packages, `set(//p:* ...)`, which is how a run re-queries its dirty set,
/// prints each failure as the last: `Skipping`, the pattern it skipped, then
/// why. The reason is often one of the first three shapes (Bazel 8.7.0 on
/// xla at `e5dd39c1c9dc` says `error loading package 'xla/mlir_hlo'` there,
/// then the repository that would not fetch), but not always: a package
/// whose load fails inside a repository rule is skipped with only `no such
/// package '@@<repo>//'`, naming the repository and not the package. So a
/// whole-package pattern (`//p:*`, `@r//p:*`) names its package whatever
/// the reason says. Before this shape was read, the line began `Skipping`
/// and matched none of the others, so the packages were never named, came
/// back empty, and were tombstoned as deletions (box run F, xla steps 12
/// and 174: the 17 packages that load `@llvm-project//mlir:tblgen.bzl` or
/// `llvm/lit_test.bzl`).
///
/// A whole-package pattern whose reason says that very package is missing
/// (`no such package 'gone': BUILD file not found`, or for an external one
/// `no such package '@@tsl//tsl/gone'` against the pattern's `@tsl`) is a
/// deletion, not a load failure, and names nothing. Any other pattern (a single target, a
/// `...` walk) names no one package, and the line is read by its reason
/// alone, as if the `Skipping '<pattern>': ` prefix were not there.
///
/// The root package comes back as `""`, which is a real package name, so the
/// answer is an `Option` rather than a non-empty string.
pub fn parse_load_error_line(line: &str) -> Option<String> {
    let l = strip_ansi(line);
    // A package the *invocation itself* declared deleted is not a failure to
    // load; see [`parse_deleted_package_line`]. It has to be tested first,
    // because its line carries a `no such package '...'` clause that would
    // otherwise be counted as a load error.
    if deleted_package_of(&l).is_some() {
        return None;
    }
    let mut rest = l.trim().strip_prefix("ERROR: ")?.trim_start();
    if let Some(r) = rest.strip_prefix("Skipping '") {
        let (pattern, why) = r.split_once("': ")?;
        let why = why.trim_start();
        match skipped_package(pattern) {
            Some(pkg) => {
                let own = why
                    .strip_prefix("no such package '")
                    .and_then(|w| w.split_once('\''))
                    .is_some_and(|(named, _)| same_package(named, pkg));
                return (!own).then(|| pkg.to_string());
            }
            None => rest = why,
        }
    }
    if let Some(r) = rest.strip_prefix("error loading package ") {
        // `'pkg': msg` — quoted, and the message itself may contain quotes.
        let r = r.trim_start().strip_prefix('\'')?;
        let end = r.find('\'')?;
        return Some(r[..end].to_string());
    }
    if let Some(r) = rest.strip_prefix("no such package ") {
        // `'@@repo//pkg': msg` — an external repository that failed to fetch
        // or configure. Counted as a load error like any other: every package
        // that depended on it is missing from the answer. Apollo's
        // `@@local_config_cuda//cuda` on a host with no CUDA is the example.
        let r = r.trim_start().strip_prefix('\'')?;
        let end = r.find('\'')?;
        return Some(r[..end].to_string());
    }
    if let Some(r) = rest.strip_prefix("package contains errors: ") {
        // `pkg: msg` — unquoted; a package name never contains a colon.
        return Some(r.split(':').next().unwrap_or(r).trim().to_string());
    }
    None
}

/// Whether Bazel's spelling of a package in a reason (`named`) is the
/// package key a pattern stands for (`pkg`).
///
/// The same text for a main-repository package. For an external one Bazel
/// prints the canonical repository (`@@tsl//tsl/gone`, or under bzlmod
/// `@@tsl+//...` or `@@+ext+tsl//...`) where the pattern, and so the key, has
/// the apparent name (`@tsl//tsl/gone`): the package paths must be equal and
/// the canonical name must end in the apparent one, after the `+` or `~`
/// Bazel splices extension and version segments in with.
fn same_package(named: &str, pkg: &str) -> bool {
    if named == pkg {
        return true;
    }
    let (Some((named_repo, named_path)), Some((repo, path))) =
        (named.split_once("//"), pkg.split_once("//"))
    else {
        return false;
    };
    if named_path != path {
        return false;
    }
    let repo = repo.trim_start_matches('@');
    let canonical = named_repo.trim_start_matches('@');
    !repo.is_empty()
        && (canonical == repo || canonical.rsplit(['+', '~']).find(|s| !s.is_empty()) == Some(repo))
}

/// The package key a skipped `<package>:*` pattern stands for, the inverse
/// of `cambium_index::label::key_pattern`: `xla/mlir_hlo` for
/// `//xla/mlir_hlo:*`, `@tsl//tsl/platform` for `@tsl//tsl/platform:*`.
/// `None` for any other pattern (a single target, or a `...` walk).
fn skipped_package(pattern: &str) -> Option<&str> {
    let pkg = pattern.strip_suffix(":*")?;
    if pkg.split('/').any(|seg| seg == "...") || pkg.contains(':') {
        return None;
    }
    if let Some(main) = pkg.strip_prefix("//") {
        return Some(main);
    }
    pkg.starts_with('@').then_some(pkg)
}

/// The package one line names as *declared deleted*, if it is such a line.
///
/// `--deleted_packages` is the invocation telling Bazel that a directory with a
/// `BUILD` file in it is not a package. Asking for one still exits 3 under
/// `--keep_going`, with:
///
/// ```text
/// ERROR: Skipping '//mobile/test/common/integration:*': no such package
/// 'mobile/test/common/integration': Package is considered deleted due to
/// --deleted_packages
/// ```
///
/// That is not a failure to load: the package does not exist for this
/// configuration, exactly as if its directory were gone, and a `//...` query —
/// which never names it — succeeds without it. Envoy's generated `repo.bazelrc`
/// is where the suite meets this.
pub fn parse_deleted_package_line(line: &str) -> Option<String> {
    deleted_package_of(&strip_ansi(line))
}

/// The deleted-package test, on already-stripped text.
fn deleted_package_of(l: &str) -> Option<String> {
    let l = l.trim();
    if !l.contains("considered deleted due to --deleted_packages") {
        return None;
    }
    // The last `no such package '<pkg>'` clause names the package itself; the
    // `Skipping '<pattern>'` clause before it names what was asked for.
    let r = l.rsplit_once("no such package ")?.1.trim_start();
    let r = r.strip_prefix('\'')?;
    let end = r.find('\'')?;
    Some(r[..end].to_string())
}

/// What a query's stderr said, in one pass: the packages that failed to load,
/// and the packages the invocation had declared deleted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StderrFacts {
    pub load_errors: LoadErrors,
    pub deleted_packages: Vec<String>,
}

impl StderrFacts {
    /// Folds one stderr line in.
    pub fn push_line(&mut self, line: &str) {
        let l = strip_ansi(line);
        if let Some(p) = deleted_package_of(&l) {
            if self.deleted_packages.len() < MAX_ERROR_PACKAGES {
                self.deleted_packages.push(p);
            }
            return;
        }
        if let Some(p) = parse_load_error_line(&l) {
            self.load_errors.push(&p);
        }
    }
}

/// Every fact in a stderr buffer. Used on a tail in tests; a live query folds
/// each line in as it arrives through [`StderrFacts::push_line`].
pub fn parse_stderr_facts(stderr: &str) -> StderrFacts {
    let mut f = StderrFacts::default();
    for line in stderr.lines() {
        f.push_line(line);
    }
    f
}

/// A target pattern as a query *word*, quoted when Bazel's query lexer would
/// not read it as one.
///
/// Bazel's query language is a language, not a list of labels: `+` is the
/// union operator, and its lexer ends a word at a `+` unless the word began
/// `@@` (the escape hatch added for canonical repository names like
/// `@@rules_jvm_external++maven+maven//:bar`). So `//bazel:libc++` — envoy's
/// `bool_flag` — lexes as `//bazel:libc`, `+`, `+` and the query is a syntax
/// error, *including* inside `set(...)`, which parses words like everywhere
/// else. A label with a `-` in a leading position, or one containing a space,
/// has the same problem.
///
/// Quoting is the lexer's own answer: a double-quoted word is taken literally.
/// The published run of 2026-09-08 is what this is for: every rule-proto
/// lookup for `//bazel:libc++`, `//bazel:libc++_enabled`, `//bazel:libstdc++`
/// and `//bazel:libstdc++_enabled` failed, so every classifier predicate that
/// needs the proto answered "no" and four benign extras were reported as
/// `unclassified` on five change sets.
pub fn query_word(pattern: &str) -> String {
    if pattern.contains('"') {
        // A label cannot contain a quote; anything that does is not a label
        // and is left for Bazel to reject on its own terms.
        return pattern.to_string();
    }
    if pattern.starts_with("@@") || !pattern.contains(['+', ' ']) {
        return pattern.to_string();
    }
    format!("\"{pattern}\"")
}

/// Every load error in a stderr buffer. Used on a tail in tests; a live query
/// counts them line by line through [`StderrFacts::push_line`].
pub fn parse_load_errors(stderr: &str) -> LoadErrors {
    let mut e = LoadErrors::default();
    for line in stderr.lines() {
        if let Some(p) = parse_load_error_line(line) {
            e.push(&p);
        }
    }
    e
}

/// Drops ANSI SGR sequences (`ESC [ ... m`), which Bazel emits unconditionally
/// and, on an error line, in the middle of the text as well as at the front.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // Skip up to and including the sequence's final byte.
        for c in chars.by_ref() {
            if c.is_ascii_alphabetic() {
                break;
            }
        }
    }
    out
}

/// How one Bazel process ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exit {
    /// The exit code, or `None` when a signal ended it.
    pub code: Option<i32>,
    pub success: bool,
}

impl Exit {
    fn of(status: ExitStatus) -> Self {
        Self {
            code: status.code(),
            success: status.success(),
        }
    }
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(c) => write!(f, "exit status: {c}"),
            None => f.write_str("terminated by a signal"),
        }
    }
}

/// A process run to completion with its output captured.
#[derive(Clone, Debug)]
pub struct Captured {
    pub exit: Exit,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// The process boundary: how a `Command` this crate built is actually run.
///
/// Everything above this trait — the frame reader, the stderr scrape, the
/// exit-3 rules, the partial verdicts the engine draws — is logic about what
/// Bazel *said*, and none of it needs a Bazel to be tested. [`ProcessRunner`]
/// spawns the binary; [`CannedRunner`] answers with bytes and an exit code
/// it was built with, so a unit test can hand `Bazel` a canned answer and
/// watch what the layers above make of it.
pub trait BazelRunner: std::fmt::Debug + Send + Sync {
    /// Runs `cmd` with both pipes attached: `on_stdout` is handed the live
    /// stdout stream while the process runs, every stderr line reaches
    /// `on_stderr` as it arrives, and the exit comes back once the process
    /// is done. A spawn failure is [`BazelError::Spawn`].
    fn stream(
        &self,
        cmd: &mut Command,
        on_stdout: &mut dyn FnMut(&mut dyn Read),
        on_stderr: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Captured>;

    /// Runs `cmd` to completion with stdout and stderr captured.
    fn output(&self, cmd: &mut Command) -> std::io::Result<Captured>;
}

/// The real thing: spawns the binary the command names.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessRunner;

impl BazelRunner for ProcessRunner {
    fn stream(
        &self,
        cmd: &mut Command,
        on_stdout: &mut dyn FnMut(&mut dyn Read),
        on_stderr: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Captured> {
        let binary = cmd.get_program().to_string_lossy().into_owned();
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|source| BazelError::Spawn { binary, source })?;
        let mut stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        // stderr is drained on its own thread: a chatty Bazel can fill the
        // pipe and deadlock the reader of stdout otherwise. The scope joins
        // it before the wait, so every line has been handed over by then.
        std::thread::scope(|s| {
            s.spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
                    on_stderr(&line);
                }
            });
            on_stdout(&mut stdout);
        });
        let status = child.wait()?;
        Ok(Captured {
            exit: Exit::of(status),
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }

    fn output(&self, cmd: &mut Command) -> std::io::Result<Captured> {
        let out = cmd.stdin(Stdio::null()).output()?;
        Ok(Captured {
            exit: Exit::of(out.status),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}

/// A runner that never spawns anything: the exit code, stdout bytes and
/// stderr text it was built with are what every command "produces". For the
/// unit tests of everything above the process boundary.
#[derive(Clone, Debug, Default)]
pub struct CannedRunner {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl CannedRunner {
    fn exit(&self) -> Exit {
        Exit {
            code: Some(self.code),
            success: self.code == 0,
        }
    }
}

impl BazelRunner for CannedRunner {
    fn stream(
        &self,
        _cmd: &mut Command,
        on_stdout: &mut dyn FnMut(&mut dyn Read),
        on_stderr: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Captured> {
        for line in self.stderr.lines() {
            on_stderr(line);
        }
        on_stdout(&mut &self.stdout[..]);
        Ok(Captured {
            exit: self.exit(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }

    fn output(&self, _cmd: &mut Command) -> std::io::Result<Captured> {
        Ok(Captured {
            exit: self.exit(),
            stdout: self.stdout.clone(),
            stderr: self.stderr.clone().into_bytes(),
        })
    }
}

/// The last [`STDERR_TAIL_LINES`] lines of a stderr stream, and the facts read
/// off every line of it — counted over the whole stream rather than the tail,
/// because a repository whose external deps do not resolve produces hundreds
/// of error lines and the tail keeps forty.
#[derive(Default)]
struct StderrTail {
    tail: VecDeque<String>,
    facts: StderrFacts,
}

impl StderrTail {
    fn push(&mut self, line: &str) {
        self.facts.push_line(line);
        if self.tail.len() == STDERR_TAIL_LINES {
            self.tail.pop_front();
        }
        self.tail.push_back(line.to_string());
    }

    fn finish(self) -> (String, StderrFacts) {
        (
            self.tail.into_iter().collect::<Vec<_>>().join("\n"),
            self.facts,
        )
    }
}

#[derive(Clone, Debug)]
pub struct Bazel {
    binary: PathBuf,
    workspace: PathBuf,
    extra_query_flags: Vec<String>,
    /// Startup options (`--output_base=...`, `--bazelrc=...`), placed before
    /// the command word on every invocation.
    startup_flags: Vec<String>,
    /// Extra environment for every invocation (`USE_BAZEL_VERSION` for a
    /// bazelisk wrapper, for example).
    env: Vec<(String, String)>,
    /// Directory for `--query_file` scratch files.
    scratch: PathBuf,
    /// What runs the commands this handle builds.
    runner: Arc<dyn BazelRunner>,
}

/// `//...:*`, plus `@name//...:*` for each in-tree external repository.
///
/// `//...:*` and not `//...`: the latter is shorthand for `//...:all`, which
/// matches rules only, and a cold index built from it would carry file labels
/// as unknown nodes while an incremental `//pkg:*` query records them as
/// source files. The same argument applies to each repository added here, so
/// each gets the same spelling.
pub fn universe_expression(repos: &[String]) -> String {
    let mut expr = String::from("//...:*");
    for r in repos {
        expr.push_str(" + @");
        expr.push_str(r);
        expr.push_str("//...:*");
    }
    expr
}

impl Bazel {
    /// Locates the workspace root by walking up from `start`, and picks the
    /// binary: explicit override, then `$BAZEL_REAL` (set when running under a
    /// bazelisk wrapper), then `$BAZEL`, then `bazelisk`, then `bazel`.
    pub fn discover(start: &Path, binary: Option<&str>) -> Result<Self> {
        let workspace = find_workspace_root(start)
            .ok_or_else(|| BazelError::NoWorkspace(start.to_path_buf()))?;
        Ok(Self::at(workspace, binary))
    }

    pub fn at(workspace: PathBuf, binary: Option<&str>) -> Self {
        let binary = binary
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("BAZEL_REAL").map(PathBuf::from))
            .or_else(|| std::env::var_os("BAZEL").map(PathBuf::from))
            .unwrap_or_else(|| {
                if which("bazelisk").is_some() {
                    PathBuf::from("bazelisk")
                } else {
                    PathBuf::from("bazel")
                }
            });
        let scratch = std::env::temp_dir();
        Self {
            binary,
            workspace,
            extra_query_flags: Vec::new(),
            startup_flags: Vec::new(),
            env: Vec::new(),
            scratch,
            runner: Arc::new(ProcessRunner),
        }
    }

    /// Runs every command this handle builds through `runner` instead of
    /// spawning the binary.
    pub fn with_runner(mut self, runner: Arc<dyn BazelRunner>) -> Self {
        self.runner = runner;
        self
    }

    /// What runs the commands this handle builds. A caller with a query shape
    /// this crate does not offer builds it on [`Bazel::command`] and runs it
    /// here, so it goes through the same seam as everything else.
    pub fn runner(&self) -> &dyn BazelRunner {
        &*self.runner
    }

    pub fn with_extra_query_flags(mut self, flags: Vec<String>) -> Self {
        self.extra_query_flags = flags;
        self
    }

    pub fn with_startup_flags(mut self, flags: Vec<String>) -> Self {
        self.startup_flags = flags;
        self
    }

    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    pub fn startup_flags(&self) -> &[String] {
        &self.startup_flags
    }

    pub fn extra_query_flags(&self) -> &[String] {
        &self.extra_query_flags
    }

    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    /// A `bazel <command>` invocation in the workspace, carrying the startup
    /// flags and environment every call here uses. Callers that need a query
    /// shape this crate does not offer (`rdeps` for verification, say) build
    /// on this rather than on a bare `Command`, so they run the same server.
    pub fn command(&self, subcommand: &str) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&self.workspace);
        cmd.args(&self.startup_flags);
        cmd.arg(subcommand);
        cmd.envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        cmd
    }

    pub fn with_scratch_dir(mut self, dir: PathBuf) -> Self {
        self.scratch = dir;
        self
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// `bazel --version`, e.g. `bazel 7.4.1`. Empty string if it cannot be
    /// determined; the version is recorded in the index header, not acted on.
    pub fn version(&self) -> String {
        // Not [`command`](Self::command): Bazel refuses `--version` after a
        // startup option (`bazel --output_base=… --version` exits 2), and the
        // benchmark always passes one, which is how a whole run recorded no
        // Bazel version at all. The workspace and the environment stay, so
        // bazelisk reads `.bazelversion` and `USE_BAZEL_VERSION` as it would.
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&self.workspace)
            .arg("--version")
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        match self.runner.output(&mut cmd) {
            Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            Err(_) => String::new(),
        }
    }

    /// Runs `bazel query` for a set of packages, streaming frames to `on_batch`.
    ///
    /// `//pkg:*` loads exactly that package and does not walk its dependencies,
    /// so the cost is proportional to the dirty set (spec 7.1).
    pub fn query_packages(
        &self,
        packages: &[String],
        on_batch: impl FnMut(&[u8], &[(usize, usize)]) -> std::result::Result<(), ReadError>,
    ) -> Result<QueryOutcome> {
        if packages.is_empty() {
            return Ok(QueryOutcome {
                exit_code: Some(0),
                success: true,
                stderr_tail: String::new(),
                targets_read: 0,
                load_errors: LoadErrors::default(),
                deleted_packages: Vec::new(),
            });
        }
        let mut expr = String::from("set(");
        for (i, p) in packages.iter().enumerate() {
            if i > 0 {
                expr.push(' ');
            }
            // A package key is a path for the main repository and
            // `@repo//pkg` for an in-tree external one; only the first needs
            // the `//` put back. See `cambium_index::label::key_pattern`.
            expr.push_str(&cambium_index::label::key_pattern(p));
        }
        expr.push(')');
        self.run_query(&expr, on_batch)
    }

    /// Runs the whole-repository query for a cold build (spec 6.9).
    ///
    /// `//...:*`, not `//...`: the latter is shorthand for `//...:all`, which
    /// matches rules only. A cold index built from it would carry file labels
    /// as unknown nodes while an incremental `//pkg:*` query records them as
    /// source files — the same graph described two different ways, which makes
    /// a cold index and a spliced one incomparable.
    /// `repos` are the in-tree external repositories stage 1 confirmed
    /// (`crate::repos`), and each adds `@name//...:*` to the expression.
    /// Their targets are real nodes of this tree written under another
    /// label, and a reverse walk that stops at the first of them is the
    /// under-report docs/XLA_MAIN_REPO_MISSES.md measures. Empty is the
    /// behaviour before Phase 5.
    pub fn query_universe(
        &self,
        repos: &[String],
        on_batch: impl FnMut(&[u8], &[(usize, usize)]) -> std::result::Result<(), ReadError>,
    ) -> Result<QueryOutcome> {
        self.run_query(&universe_expression(repos), on_batch)
    }

    fn run_query(
        &self,
        expr: &str,
        on_batch: impl FnMut(&[u8], &[(usize, usize)]) -> std::result::Result<(), ReadError>,
    ) -> Result<QueryOutcome> {
        let mut cmd = self.command("query");
        let _scratch_file = self.expression_arg(&mut cmd, expr, "query")?;

        for f in QUERY_FLAGS.iter().chain(HYGIENE.iter()) {
            cmd.arg(f);
        }
        for f in &self.extra_query_flags {
            cmd.arg(f);
        }

        tracing::debug!(
            binary = %self.binary.display(),
            expr_len = expr.len(),
            query = %truncate(expr, 200),
            "running bazel query"
        );

        let mut on_batch = on_batch;
        let mut targets_read: std::result::Result<usize, ReadError> = Ok(0);
        let mut stderr = StderrTail::default();
        let done = self.runner.stream(
            &mut cmd,
            &mut |stdout| {
                targets_read = read_frames(
                    BufReader::with_capacity(1 << 20, stdout),
                    cambium_proto::reader::DEFAULT_BATCH,
                    &mut on_batch,
                );
            },
            &mut |line| stderr.push(line),
        )?;
        let (stderr_tail, facts) = stderr.finish();

        // A read error usually means bazel died mid-stream; the stderr tail is
        // the useful part of that story, so report it rather than the truncation.
        let targets_read = match targets_read {
            Ok(n) => n,
            Err(e) => {
                if done.exit.success {
                    return Err(e.into());
                }
                tracing::debug!(error = %e, "bazel stream ended early");
                0
            }
        };

        Ok(QueryOutcome {
            exit_code: done.exit.code,
            success: done.exit.success,
            stderr_tail,
            targets_read,
            load_errors: facts.load_errors,
            deleted_packages: facts.deleted_packages,
        })
    }

    /// `rbuildfiles(...)`: what loads any of `bzl_paths` (spec 7.2). Paths are
    /// workspace-relative; see [`Rbuildfiles`] for why the answer is not just
    /// a list of packages. The `bool` says Sky Query finished with errors, so
    /// the answer is whatever it managed.
    ///
    /// **Oracle only.** The engine reads this out of the index's static load
    /// closure instead; nothing on the `affected` path calls Bazel here. A
    /// partial `rbuildfiles` can be missing exactly the package that loads the
    /// changed file, and an under-approximation is unusable for an answer. The
    /// comparison suite can live with one, because it only asks whether
    /// cambium's answer *contains* Bazel's.
    pub fn rbuildfiles(&self, bzl_paths: &[String]) -> Result<(Rbuildfiles, bool)> {
        if bzl_paths.is_empty() {
            return Ok((Rbuildfiles::default(), false));
        }
        let expr = format!("rbuildfiles({})", bzl_paths.join(", "));
        let mut cmd = self.command("query");
        cmd.arg(&expr)
            // rbuildfiles needs Sky Query mode, which --universe_scope enables.
            .arg("--universe_scope=//...")
            .arg("--order_output=no")
            .arg("--output=label")
            .arg("--keep_going");
        for f in HYGIENE {
            cmd.arg(f);
        }

        let mut out = String::new();
        let mut read: std::io::Result<usize> = Ok(0);
        let mut stderr = StderrTail::default();
        let done = self.runner.stream(
            &mut cmd,
            &mut |stdout| read = stdout.read_to_string(&mut out),
            &mut |line| stderr.push(line),
        )?;
        read?;
        let (stderr_tail, _) = stderr.finish();
        // Exit 3 is "finished with errors" under --keep_going: the answer is
        // whatever loaded. Whether that is usable is the caller's call.
        if done.exit.code == Some(3) {
            return Ok((parse_buildfile_labels(&out), true));
        }
        if !done.exit.success {
            return Err(BazelError::Failed {
                args: truncate(&expr, 200),
                status: done.exit.to_string(),
                stderr: stderr_tail,
            });
        }
        Ok((parse_buildfile_labels(&out), false))
    }

    /// `bazel query '@R//...' --output=package` for one repository: the
    /// package list that says whether Bazel agrees with what the tree claims
    /// the repository is.
    ///
    /// Run at cold build, *before* the universe query and never between it
    /// and the loading-layer dump: it is a Bazel command with its own option
    /// set, and one of those between the two costs the dump packages.
    ///
    /// Best-effort, like [`Bazel::external_rules`], with one difference: a
    /// non-zero exit whose stdout still lists packages is kept, because a
    /// repository that loaded while an unrelated one failed is the common
    /// case on a large tree. The caller decides what a missing or partial
    /// answer means; here it is only reported.
    pub fn probe_repo(&self, repo: &str) -> Option<RepoProbe> {
        let mut cmd = self.command("query");
        // `//...:*`, not `//...`: the latter is shorthand for `//...:all`,
        // which matches rules only, so a package holding nothing but source
        // files contributes no target and drops out of the list entirely.
        // The claim is then checked against a package set short by exactly
        // those packages and refused: envoy's `@envoy_api` has two of them
        // (`examples/service_envoy` and one more), and 533 was compared
        // against the directory's 535. The same argument as
        // `Bazel::query_universe`, for the same reason.
        cmd.arg(format!("@{repo}//...:*")).arg("--output=package");
        for f in HYGIENE {
            cmd.arg(f);
        }
        let o = match self.runner.output(&mut cmd) {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!(error = %e, repo, "the repository probe could not run");
                return None;
            }
        };
        let stdout = String::from_utf8_lossy(&o.stdout);
        let stderr = String::from_utf8_lossy(&o.stderr);
        let packages: Vec<String> = stdout
            .lines()
            .filter_map(|l| package_of_repo(l.trim(), repo))
            .collect();
        // Whether any package Bazel could not load belongs to this
        // repository. The failure carries the label in whichever spelling
        // this Bazel prints, so it is read the same way.
        let load_error = stderr
            .lines()
            .any(|l| parse_load_error_line(l).is_some_and(|p| package_of_repo(&p, repo).is_some()));
        if !o.exit.success && packages.is_empty() {
            tracing::debug!(status = %o.exit, repo, "the repository probe listed no package");
        }
        Some(RepoProbe {
            repo: repo.to_string(),
            packages,
            load_error,
        })
    }

    /// The external repositories whose rules depend on `repo`'s targets, and
    /// the packages of `repo` they depend on.
    ///
    /// Two queries at cold build, for an in-tree repository the universe was
    /// widened by. The first is the bridge: every rule Bazel reaches from
    /// `//...` on its way down to `@repo//...`, less the two ends,
    /// `kind(rule, rdeps(//..., @repo//...:*)) except //... except
    /// @repo//...:*`. On envoy at its pin that is 101 rules, all of `@grpc`:
    /// its xDS client is built from the API's protos. The second is what the
    /// bridge depends on inside `@repo`, transitively ([`consumed_expr`]),
    /// whose packages say which of the repository's packages a consumer
    /// cares about. The packages are pooled across consumers rather than
    /// split per consumer (one query, not one per consumer), which is exact
    /// when there is one consumer and wide when there are several; which
    /// leaf of a consumer reaches what is [`Bazel::leaf_reaches`].
    ///
    /// Bazel prints canonical names (`@@grpc+//...`); the consumer is recorded
    /// by its apparent name, which is how the index names repositories and
    /// how the walk matches labels ([`apparent_of_canonical`]).
    ///
    /// Best-effort: a query that will not run is a debug line and `None`, and
    /// the caller keeps the index it was building, without the table.
    pub fn consumer_bridge(&self, repo: &str) -> Option<Vec<(String, BTreeSet<String>)>> {
        let run = |expr: String| -> Option<Vec<String>> {
            let mut cmd = self.command("query");
            cmd.arg(&expr).arg("--output=label").arg("--keep_going");
            for f in HYGIENE {
                cmd.arg(f);
            }
            let o = match self.runner.output(&mut cmd) {
                Ok(o) => o,
                Err(e) => {
                    tracing::debug!(error = %e, repo, "the consumer query could not run");
                    return None;
                }
            };
            if !o.exit.success && o.exit.code != Some(3) {
                tracing::debug!(status = %o.exit, repo, expr, "the consumer query failed");
                return None;
            }
            let lines: Vec<String> = String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect();
            tracing::debug!(status = %o.exit, repo, lines = lines.len(), stderr_len = o.stderr.len(), "consumer query answered");
            Some(lines)
        };
        let bridge = run(format!(
            "kind(rule, rdeps(//..., @{repo}//...:*)) except //... except @{repo}//...:*"
        ))?;
        if bridge.is_empty() {
            return Some(Vec::new());
        }
        let mut consumers: BTreeSet<String> = BTreeSet::new();
        for l in &bridge {
            if let Some(name) = label_repo_token(l) {
                consumers.insert(apparent_of_canonical(name));
            }
        }
        let used = run(consumed_expr(repo, &bridge)).unwrap_or_default();
        let packages: BTreeSet<String> = used
            .iter()
            .filter_map(|l| package_of_repo(l, repo))
            .collect();
        Some(
            consumers
                .into_iter()
                .map(|c| (c, packages.clone()))
                .collect(),
        )
    }

    /// Which packages of `repo` each of these leaves reaches through its
    /// transitive deps, and what outside `repo` it reaches them through:
    /// leaf -> [`LeafReach`], a leaf that reaches nothing of the repository
    /// with an empty set of packages.
    ///
    /// The leaves are the labels of a consumer repository that an index
    /// holds, spelled as the index spells them (`@grpc//:grpc++`). One query
    /// answers all of them, the leaves' closure with `--output=graph`
    /// ([`leaf_expr`]), walked forward from each leaf on its own, so that
    /// what `deps(set(leaves)) intersect @repo//...:*` would pool is
    /// attributed per leaf. The closure rather than `allpaths(set(leaves),
    /// @repo//...:*)`: the paths into the repository are the answer today,
    /// and the rules that could carry a new one after a `BUILD` edit are the
    /// rest of the closure, which is what [`LeafReach::via`] records. On
    /// envoy at a WORKSPACE-era commit (123fa2874678), 130 leaves, a closure
    /// of 2859 nodes and 63840 edges (5.5 MB of graph) through 55 main-repo
    /// packages, the query took 0.5 s on a warm server and 28 s on one that
    /// had to load the workspace first.
    ///
    /// A leaf missing from the answer is missing from the map, which the
    /// caller reads as not known; the whole answer is `None` when the query
    /// will not run. Exit 3 (`--keep_going` over a closure with load errors,
    /// which envoy's `@grpc` has on every platform through `@openssl`) is an
    /// answer, as it is for [`Bazel::consumer_bridge`]: a leaf whose path
    /// runs through the package that failed is recorded as reaching what
    /// Bazel could load.
    pub fn leaf_reaches(
        &self,
        repo: &str,
        leaves: &[String],
    ) -> Option<BTreeMap<String, LeafReach>> {
        if leaves.is_empty() {
            return Some(BTreeMap::new());
        }
        let started = std::time::Instant::now();
        let graph = self.text_query(
            &leaf_expr(repo, leaves),
            &[
                "--output=graph",
                "--nograph:factored",
                "--graph:node_limit=-1",
            ],
            "leaf map",
        )?;
        let mut out = parse_leaf_reach(&graph, repo, leaves);
        // The main-repo files the closure's packages load are part of what
        // the map was derived through, pooled over the leaves: a rule class
        // defined there can give an external rule an implicit edge into the
        // repository ([`closure_loads_via`]).
        let loads = self.closure_loads(leaves, &BTreeSet::new(), &[])?;
        for reach in out.values_mut() {
            reach.via.extend(loads.iter().cloned());
        }
        tracing::debug!(
            repo,
            leaves = leaves.len(),
            resolved = out.len(),
            reaching = out.values().filter(|r| !r.packages.is_empty()).count(),
            via = out
                .values()
                .flat_map(|r| &r.via)
                .collect::<BTreeSet<_>>()
                .len(),
            loads = loads.len(),
            bytes = graph.len(),
            ms = started.elapsed().as_millis() as u64,
            "leaf query answered"
        );
        Some(out)
    }

    /// Which targets inside the index's universe each of these leaves
    /// reaches through targets outside it alone: leaf -> the labels, spelled
    /// as the index spells them, of the first inside node on each such path;
    /// and which repositories each leaf's closure runs through on the way
    /// ([`RoundTrips::closure`]). A leaf that reaches nothing inside, or that
    /// Bazel could not resolve, is left out of the edges; the whole answer is
    /// `None` when a query will not run.
    ///
    /// The leaves are the external labels the index holds that no package
    /// of it owns, and the universe is `//...:*` plus `indexed`, the in-tree
    /// repositories the index holds the packages of. Two questions, both
    /// `--keep_going` (exit 3 is an answer, as it is for
    /// [`Bazel::leaf_reaches`]):
    ///
    /// 1. `deps(set(leaves))` with `--output=graph`: the leaves' closure,
    ///    walked forward from each leaf on its own through the nodes outside
    ///    the universe. Where a walk meets a node inside it records the node
    ///    and goes no further ([`parse_round_trips`]); every repository it
    ///    passes through is the leaf's closure ([`parse_closure_repos`]).
    ///    No `//...:*` in it, so a server that has not loaded the repository
    ///    does not load it for this.
    /// 2. `loadfiles` over the same closure less `P`, the packages of the
    ///    universe among its nodes ([`Bazel::closure_loads`]): which files
    ///    of the main repository or an indexed one the packages outside
    ///    load. A rule class or macro defined there decides what an external
    ///    target depends on, so an edit to one moves the answer with nothing
    ///    outside moving ([`RoundTrips::via`]).
    ///
    /// The closure was asked with `--output=package` once, and the paths
    /// into `P` with a third question, `allpaths`, when `P` was not empty.
    /// The graph answers both, and it is what says which leaf runs through
    /// which repository: envoy's `@grpc//:grpc++` through `@c-ares`, whose
    /// patch is a file of the main repository that no main-repo rule's
    /// closure names by a label.
    ///
    /// On `bazelbuild/rules_shell` at `b1eed28` (Bazel 9.1.1, 29 leaves) the
    /// closure is 47 packages, two of them the main repository's, and the
    /// graph names `@bazel_tools//tools/test:collect_coverage` reaching
    /// `//shell/runfiles:runfiles` and `//shell:toolchain_type`: the root
    /// module's own runfiles library, which every test depends on through
    /// the coverage collector.
    pub fn round_trips(&self, indexed: &[String], leaves: &[String]) -> Option<RoundTrips> {
        if leaves.is_empty() {
            return Some(RoundTrips::default());
        }
        let started = std::time::Instant::now();
        let graph = self.text_query(
            &format!("deps({})", quoted_set(leaves)),
            &[
                "--output=graph",
                "--nograph:factored",
                "--graph:node_limit=-1",
            ],
            "round-trip closure",
        )?;
        let g = LabelGraph::parse(&graph);
        let packages = package_lines(&g);
        let patterns = inside_patterns(&packages, indexed);
        // Asked even when nothing comes back in today: an edit to one of
        // these files is how a leaf that reaches nothing starts to.
        let via = self.closure_loads(leaves, &patterns, indexed)?;
        let reach = if patterns.is_empty() {
            BTreeMap::new()
        } else {
            parse_round_trips(&g, indexed, leaves)
        };
        let closure = parse_closure_repos(&g, indexed, leaves);
        tracing::debug!(
            leaves = leaves.len(),
            nodes = g.names.len(),
            bytes = graph.len(),
            packages = packages.lines().count(),
            inside_packages = patterns.len(),
            reaching = reach.len(),
            entries = reach.values().map(BTreeSet::len).sum::<usize>(),
            repositories = closure.values().flatten().collect::<BTreeSet<_>>().len(),
            via = via.len(),
            ms = started.elapsed().as_millis() as u64,
            "round-trip query answered"
        );
        Some(RoundTrips {
            reach,
            via,
            closure,
        })
    }

    /// The files of the main repository and of the `indexed` repositories
    /// that the packages of the leaves' closure load, less the packages
    /// `except` names (package patterns, `//pkg:*`), as via entries
    /// ([`closure_loads_via`]). `None` when the query will not run: the
    /// caller then knows nothing about what its answer was derived through,
    /// and must not keep the answer as if it did.
    ///
    /// The index's own packages carry their loads already, so a caller
    /// whose closure runs through them excludes them; a caller that does
    /// not gets their loads too, which only makes it ask again more often.
    pub fn closure_loads(
        &self,
        leaves: &[String],
        except: &BTreeSet<String>,
        indexed: &[String],
    ) -> Option<BTreeSet<String>> {
        if leaves.is_empty() {
            return Some(BTreeSet::new());
        }
        let loads = self.text_query(
            &loads_expr(leaves, except),
            &["--output=label"],
            "closure loads",
        )?;
        Some(closure_loads_via(&loads, indexed))
    }

    /// Puts `expr` on `cmd`: as an argument, or through `--query_file` when
    /// it is long, since argv limits are OS-dependent and a dirty set of
    /// thousands of packages will exceed them. The file lives as long as the
    /// returned guard, named after `stem`.
    fn expression_arg(
        &self,
        cmd: &mut Command,
        expr: &str,
        stem: &str,
    ) -> std::io::Result<Option<ScratchFile>> {
        if expr.len() <= MAX_INLINE_QUERY {
            cmd.arg(expr);
            return Ok(None);
        }
        let path = self.scratch.join(format!(
            "cambium-{stem}-{}-{}.txt",
            std::process::id(),
            now_nanos()
        ));
        std::fs::write(&path, expr)?;
        cmd.arg(format!("--query_file={}", path.display()));
        Ok(Some(ScratchFile(path)))
    }

    /// One `bazel query` whose stdout is text, with the flags that define the
    /// graph (`extra_query_flags`, `--keep_going`) and `args`; the expression
    /// goes through `--query_file` when it is long. `None`, with a debug
    /// line naming `what`, when the command will not run or fails other than
    /// with exit 3.
    fn text_query(&self, expr: &str, args: &[&str], what: &str) -> Option<String> {
        let mut cmd = self.command("query");
        let _scratch = match self.expression_arg(&mut cmd, expr, "query") {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, what, "the query file could not be written");
                return None;
            }
        };
        cmd.args(args)
            .arg("--keep_going")
            .arg("--order_output=no")
            .args(HYGIENE)
            .args(&self.extra_query_flags);
        let o = match self.runner.output(&mut cmd) {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!(error = %e, what, "the query could not run");
                return None;
            }
        };
        if !o.exit.success && o.exit.code != Some(3) {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let last = stderr
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            tracing::debug!(status = %o.exit, what, last, "the query failed");
            return None;
        }
        Some(String::from_utf8_lossy(&o.stdout).into_owned())
    }

    /// `bazel query '//external:*' --output=build`, which prints every
    /// repository the WORKSPACE evaluation declared, rule kind and attributes
    /// and all.
    ///
    /// The one way to learn that tensorflow's `tf_vendored(name = "xla", path
    /// = "third_party/xla")` puts `@xla` at `third_party/xla`: the rule is a
    /// custom `repository_rule` whose implementation symlinks a path it
    /// computes, so no static scan of the `.bzl` can say where it lands, but
    /// Bazel has already run it and will say so.
    ///
    /// Best-effort, like [`Bazel::repo_mapping_dump`]: the query is meaningless
    /// under bzlmod, deprecated, and can fail for reasons that have nothing to
    /// do with the caller. Anything that goes wrong is a debug line and
    /// `None`, and the caller is left knowing no less than it did before.
    pub fn external_rules(&self) -> Option<String> {
        let mut cmd = self.command("query");
        cmd.arg("//external:*").arg("--output=build");
        for f in HYGIENE {
            cmd.arg(f);
        }
        match self.runner.output(&mut cmd) {
            Ok(o) if o.exit.success => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
            Ok(o) => {
                tracing::debug!(
                    status = %o.exit,
                    "bazel query //external:* did not succeed; nested repositories stay unmapped"
                );
                None
            }
            Err(e) => {
                tracing::debug!(error = %e, "bazel query //external:* could not run");
                None
            }
        }
    }

    /// `bazel dump --skyframe=deps`, filtered to a set of key kinds.
    ///
    /// This is Bazel's own loading-phase graph, on the server this handle has
    /// just loaded the repository into, and it is what
    /// `cambium_index::sky` stores. It has to run on the *same* server as the
    /// query that loaded the packages: a fresh server has an empty Skyframe
    /// graph and would dump nothing at all.
    ///
    /// `filter` is a comma-separated list of regexes matching the `KIND:`
    /// prefix, a `-` prefix excluding. Failure is an error with Bazel's own
    /// stderr in it, never an empty answer: an index without a layer is one
    /// no later run can be answered from, so the caller has to be told rather
    /// than handed an index that silently says nothing is dirty.
    pub fn dump_skyframe(&self, filter: &str) -> Result<String> {
        // No `HYGIENE` here: `dump` is a debugging command and rejects the
        // progress flags every other invocation passes (`ERROR:
        // --noshow_loading_progress :: Unrecognized option`), which is a hard
        // exit rather than a warning.
        let mut cmd = self.command("dump");
        cmd.arg("--skyframe=deps")
            .arg(format!("--skykey_filter={filter}"));
        let out = self.runner.output(&mut cmd)?;
        if !out.exit.success {
            return Err(BazelError::Failed {
                args: format!("dump --skyframe=deps --skykey_filter={filter}"),
                status: out.exit.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Loads every package in the repository, keeping nothing.
    ///
    /// `//...:*` again, but as a plain label query rather than the streamed
    /// proto the index is built from. The difference matters for one reason:
    /// after the streamed query a server can hold the loading graph's nodes
    /// without their values, and `dump --skyframe=deps` prints only values, so
    /// the loading layer comes back with its packages missing. This query
    /// materialises them. The output is thrown away.
    ///
    /// Warm by construction (it runs on the server that just answered the same
    /// pattern), and `--keep_going` so a repository with a package that cannot
    /// load is loaded as far as it goes, exactly as the index's own query is.
    pub fn load_universe(&self, repos: &[String]) -> Result<()> {
        let expr = universe_expression(repos);
        let mut cmd = self.command("query");
        cmd.arg(&expr).arg("--output=label").arg("--keep_going");
        for f in HYGIENE {
            cmd.arg(f);
        }
        let out = self.runner.output(&mut cmd)?;
        // Exit 3 is `--keep_going` reporting that something did not load,
        // which is the same answer the index's own query accepts.
        match out.exit.code {
            Some(0) | Some(3) => Ok(()),
            _ => Err(BazelError::Failed {
                args: format!("query {expr} --output=label"),
                status: out.exit.to_string(),
                stderr: truncate(&String::from_utf8_lossy(&out.stderr), 2000),
            }),
        }
    }

    /// Loads the named packages, keeping nothing.
    ///
    /// [`Bazel::load_universe`] narrowed to a package list, for the refresh
    /// path: only the packages a splice query re-queried can have come back
    /// without their values, and `set(//a:* //b:*)` is a fraction of `//...`.
    /// The expression goes through a query file when it is long, exactly as
    /// the splice query's does.
    pub fn load_packages(&self, packages: &[String]) -> Result<()> {
        if packages.is_empty() {
            return Ok(());
        }
        let expr = format!(
            "set({})",
            packages
                .iter()
                .map(|p| cambium_index::label::key_pattern(p))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let mut cmd = self.command("query");
        let _scratch = self.expression_arg(&mut cmd, &expr, "reload")?;
        cmd.arg("--output=label").arg("--keep_going");
        for f in HYGIENE {
            cmd.arg(f);
        }
        let out = self.runner.output(&mut cmd)?;
        match out.exit.code {
            Some(0) | Some(3) => Ok(()),
            _ => Err(BazelError::Failed {
                args: format!("query {} --output=label", truncate(&expr, 200)),
                status: out.exit.to_string(),
                stderr: truncate(&String::from_utf8_lossy(&out.stderr), 2000),
            }),
        }
    }

    /// The server's output base, where external repositories are checked out.
    ///
    /// Only used to keep the machine's own paths out of the stored loading
    /// layer, so an answer that cannot be had costs a few machine-specific
    /// strings in the payload and nothing else.
    pub fn output_base(&self) -> Option<String> {
        let mut cmd = self.command("info");
        cmd.arg("output_base");
        match self.runner.output(&mut cmd) {
            Ok(o) if o.exit.success => {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                (!s.is_empty()).then_some(s)
            }
            Ok(o) => {
                tracing::debug!(status = %o.exit, "bazel info output_base did not succeed");
                None
            }
            Err(e) => {
                tracing::debug!(error = %e, "could not run bazel info output_base");
                None
            }
        }
    }

    /// The root module's repository mapping as Bazel prints it, for
    /// `cambium_index::RepoMapping::from_dump` to parse.
    ///
    /// `bazel mod dump_repo_mapping ""` (Bazel >= 7.1) prints it as one JSON
    /// object on stdout: apparent repository name -> canonical name. Measured
    /// output shapes per Bazel version are recorded in `cambium_index::loads`. Anything that goes wrong — the subcommand does
    /// not exist, the repository is in WORKSPACE mode, the module graph will
    /// not resolve — is a warning and the identity mapping, which is what
    /// WORKSPACE mode means anyway. The mapping only ever *names* external
    /// loads; nothing walks into them, so a wrong name costs nothing but a
    /// less precise record.
    pub fn repo_mapping_dump(&self) -> Option<String> {
        let mut cmd = self.command("mod");
        cmd.arg("dump_repo_mapping").arg("");
        match self.runner.output(&mut cmd) {
            Ok(o) if o.exit.success => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
            Ok(o) => {
                // Once per process: a replay asks for the mapping on every
                // step, and a WORKSPACE repository answers the same way each
                // time.
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    tracing::warn!(
                        status = %o.exit,
                        "bazel mod dump_repo_mapping failed (WORKSPACE mode, or bazel < 7.1); \
                         falling back to the identity repository mapping"
                    )
                });
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not run bazel mod dump_repo_mapping");
                None
            }
        }
    }

    /// Stops the Bazel server behind this output base, if one is running.
    ///
    /// Best effort by design: it is called on the way out of a run, where a
    /// failure has nothing left to spoil, so it is logged and not returned.
    /// A server that is not running is not an error either; `bazel shutdown`
    /// exits 0 without starting one.
    pub fn shutdown(&self) {
        let mut cmd = self.command("shutdown");
        match self.runner.output(&mut cmd) {
            Ok(o) if o.exit.success => {}
            Ok(o) => tracing::debug!(status = %o.exit, "bazel shutdown did not succeed"),
            Err(e) => tracing::debug!(error = %e, "could not run bazel shutdown"),
        }
    }
}

/// What `rbuildfiles` said, split by what cambium can act on.
///
/// `rbuildfiles` answers with labels, and only some of them name a main-repo
/// package. A `.bzl` defining repository rules comes back naming the
/// *external* repository it builds, or `//external:WORKSPACE`, and never a
/// package — so flattening the answer to "no packages" would mark nothing
/// dirty while the contents of every external repository changed underneath.
///
/// Observed on 9.2.0 / 8.5.1 / 7.4.1 against a workspace whose `//app` target
/// depends on `@my_repo//:x`:
///
/// | mode | query | answer |
/// |---|---|---|
/// | bzlmod (all three) | `rbuildfiles(bazel/ext.bzl)` | `@my_repo//:BUILD` |
/// | bzlmod (all three) | `rbuildfiles(MODULE.bazel)` | *(empty)* |
/// | bzlmod (all three) | `rbuildfiles(WORKSPACE)` | every package, `@my_repo//:BUILD`, `@bazel_tools//tools/genrule:BUILD` |
/// | WORKSPACE (7.4.1, 8.5.1 with `--noenable_bzlmod --enable_workspace`) | `rbuildfiles(bazel/ext.bzl)` | `//external:WORKSPACE`, `@my_repo//:BUILD`, every package |
/// | either, any version | `rbuildfiles(<.bzl nothing loads>)` | *(empty)* |
///
/// So a repository-rule `.bzl` is never silent, and a genuinely dead one is
/// the only thing that comes back empty — which is why anything non-empty
/// that is not a main-repo package is treated as global.
///
/// Oracle only: the engine reads the same answer out of the index's static
/// load closure (`cambium_index::loads`). The table is kept because it is what
/// the oracle (`cambium_compare::bzloracle`) is calibrated against.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rbuildfiles {
    /// Main-repository packages whose `BUILD` loads one of the paths — the
    /// set the engine now reads out of the load closure instead, and that the
    /// comparison suite checks against this.
    pub packages: Vec<String>,
    /// Labels that name a loader outside any main-repo package:
    /// `//:MODULE.bazel`, `//:WORKSPACE[.bazel|.bzlmod]`, `//external:*`, and
    /// any `@repo//...` label. Non-empty means the change reaches external
    /// repository contents, which cambium's index does not model — global.
    pub workspace_level: Vec<String>,
}

impl Rbuildfiles {
    /// Nothing at all loads the path: not a package, not the workspace files,
    /// not an external repository. A dead file.
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty() && self.workspace_level.is_empty()
    }
}

/// Parses `--output=label` lines from `rbuildfiles`.
///
/// `//lib/core:BUILD` -> a package; `@my_repo//:BUILD`, `//external:WORKSPACE`,
/// `//:MODULE.bazel` -> workspace-level. Other shapes are skipped.
pub fn parse_buildfile_labels(out: &str) -> Rbuildfiles {
    const WORKSPACE_FILES: &[&str] = &[
        "MODULE.bazel",
        "MODULE.bazel.lock",
        "WORKSPACE",
        "WORKSPACE.bazel",
        "WORKSPACE.bzlmod",
        "REPO.bazel",
    ];
    let mut r = Rbuildfiles::default();
    for line in out.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        // Any label in another repository: `@repo//...` or `@@canonical//...`.
        if l.starts_with('@') {
            r.workspace_level.push(l.to_string());
            continue;
        }
        let Some(rest) = l.strip_prefix("//") else {
            continue;
        };
        let Some((pkg, name)) = rest.rsplit_once(':') else {
            continue;
        };
        // `//external:...` is the pseudo-package Bazel reports WORKSPACE-driven
        // loads under in WORKSPACE mode; `//:MODULE.bazel` and friends are the
        // workspace files themselves.
        let external_pseudo_package = pkg == "external" || pkg.starts_with("external/");
        let workspace_file = pkg.is_empty() && WORKSPACE_FILES.contains(&name);
        if external_pseudo_package || workspace_file {
            r.workspace_level.push(l.to_string());
        } else if name == "BUILD" || name == "BUILD.bazel" {
            r.packages.push(pkg.to_string());
        }
    }
    r.packages.sort();
    r.packages.dedup();
    r.workspace_level.sort();
    r.workspace_level.dedup();
    r
}

/// Walks up from `start` looking for a workspace marker.
pub fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    const MARKERS: &[&str] = &[
        "MODULE.bazel",
        "WORKSPACE",
        "WORKSPACE.bazel",
        "WORKSPACE.bzlmod",
    ];
    let mut dir = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    loop {
        if MARKERS.iter().any(|m| dir.join(m).exists()) {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

struct ScratchFile(PathBuf);

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let p = dir.join(bin);
        p.is_file().then_some(p)
    })
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}... ({} bytes)", &s[..n], s.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim from a `bazel 9.2.0 query //...:*` over ApolloAuto/apollo,
    /// colour codes and all: the repository's deps live in WORKSPACE, which
    /// Bazel 9 no longer reads, so nearly every package fails to load.
    const APOLLO_STDERR: &str = concat!(
        "\u{1b}[31m\u{1b}[1mERROR: \u{1b}[0merror loading package 'modules/planning/traffic_rules/crosswalk': at /w/tools/cpplint.bzl:3:6: Unable to find package for @@[unknown repo 'rules_python' requested from @@]//python:defs.bzl\n",
        "\u{1b}[31m\u{1b}[1mERROR: \u{1b}[0merror loading package '': compilation of module 'tools/install/install.bzl' failed\n",
        "\u{1b}[31m\u{1b}[1mERROR: \u{1b}[0mpackage contains errors: third_party/bazel: name 'py_library' is not defined (did you mean 'cc_library'?)\n",
        "\u{1b}[35mWARNING: \u{1b}[0m--keep_going specified, ignoring errors. Results may be inaccurate\n",
    );

    fn rule(name: &str) -> cambium_proto::blaze_query::Target {
        use cambium_proto::blaze_query::{target::Discriminator, Rule, Target};
        Target {
            r#type: Discriminator::Rule as i32,
            rule: Some(Rule {
                name: name.to_string(),
                rule_class: "cc_library".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn canned(code: i32, stdout: Vec<u8>, stderr: &str) -> Bazel {
        Bazel::at(std::env::temp_dir(), Some("bazel")).with_runner(Arc::new(CannedRunner {
            code,
            stdout,
            stderr: stderr.to_string(),
        }))
    }

    /// Everything a query reads off a process — the frames, the exit, the
    /// tail, the facts scraped from every stderr line — through a runner
    /// that never spawned one.
    #[test]
    fn a_query_through_a_canned_runner_reads_frames_and_scrapes_stderr() {
        let bytes = cambium_proto::encode_stream(&[rule("//a:x"), rule("//a:y")]);
        let bazel = canned(3, bytes, APOLLO_STDERR);
        let mut frames = 0;
        let out = bazel
            .query_packages(&["a".into()], |_, ranges| {
                frames += ranges.len();
                Ok(())
            })
            .unwrap();
        assert_eq!(frames, 2);
        assert_eq!(out.targets_read, 2);
        assert_eq!(out.exit_code, Some(3));
        assert!(!out.success);
        assert_eq!(out.load_errors.count, 3);
        assert!(
            out.stderr_tail.contains("py_library"),
            "{}",
            out.stderr_tail
        );
    }

    #[test]
    fn a_universe_that_exits_zero_is_a_success_with_no_facts() {
        let bazel = canned(0, cambium_proto::encode_stream(&[rule("//a:x")]), "");
        let out = bazel.query_universe(&[], |_, _| Ok(())).unwrap();
        assert!(out.success);
        assert_eq!(out.exit_code, Some(0));
        assert!(out.load_errors.is_empty());
        assert!(out.deleted_packages.is_empty());
    }

    /// A stream that ends mid-frame under a successful exit is a read error;
    /// under a failed exit it is reported as the failure it came with.
    #[test]
    fn a_truncated_stream_is_an_error_only_when_bazel_claimed_success() {
        let mut bytes = cambium_proto::encode_stream(&[rule("//a:x")]);
        bytes.truncate(bytes.len() - 2);
        assert!(matches!(
            canned(0, bytes.clone(), "").query_universe(&[], |_, _| Ok(())),
            Err(BazelError::Read(_))
        ));
        let out = canned(1, bytes, "ERROR: boom")
            .query_universe(&[], |_, _| Ok(()))
            .unwrap();
        assert_eq!(out.exit_code, Some(1));
        assert_eq!(out.targets_read, 0);
        assert_eq!(out.stderr_tail, "ERROR: boom");
    }

    #[test]
    fn rbuildfiles_through_a_canned_runner_splits_loaders_and_reports_exit_3() {
        let bazel = canned(3, b"//lib:BUILD\n@x//:BUILD\n".to_vec(), "");
        let (r, partial) = bazel.rbuildfiles(&["a.bzl".into()]).unwrap();
        assert!(partial);
        assert_eq!(r.packages, vec!["lib".to_string()]);
        assert_eq!(r.workspace_level, vec!["@x//:BUILD".to_string()]);
        let err = canned(1, Vec::new(), "ERROR: nope")
            .rbuildfiles(&["a.bzl".into()])
            .unwrap_err();
        assert!(matches!(err, BazelError::Failed { ref stderr, .. } if stderr == "ERROR: nope"));
    }

    #[test]
    fn the_version_and_the_mapping_dump_come_off_the_runner_too() {
        let bazel = canned(0, b"bazel 7.4.1\n".to_vec(), "");
        assert_eq!(bazel.version(), "bazel 7.4.1");
        assert_eq!(bazel.repo_mapping_dump().as_deref(), Some("bazel 7.4.1\n"));
        assert_eq!(canned(2, Vec::new(), "").repo_mapping_dump(), None);
    }

    #[test]
    fn a_binary_that_cannot_be_spawned_is_named() {
        let bazel = Bazel::at(std::env::temp_dir(), Some("/nonexistent/cambium-bazel"));
        let err = bazel.query_universe(&[], |_, _| Ok(())).unwrap_err();
        assert!(
            matches!(err, BazelError::Spawn { ref binary, .. } if binary.contains("cambium-bazel")),
            "{err}"
        );
    }

    #[test]
    fn a_label_the_query_lexer_would_split_is_quoted() {
        // envoy's `bool_flag`s and the `config_setting`s over them: `+` is the
        // union operator, so unquoted these are three tokens, not one.
        assert_eq!(query_word("//bazel:libc++"), "\"//bazel:libc++\"");
        assert_eq!(
            query_word("//bazel:libstdc++_enabled"),
            "\"//bazel:libstdc++_enabled\""
        );
        // Ordinary labels are left alone, so the queries the suite already
        // sends do not change shape.
        assert_eq!(query_word("//bazel:envoy"), "//bazel:envoy");
        assert_eq!(query_word("//:BUILD"), "//:BUILD");
        // A canonical repository name already lexes as one word, and quoting
        // it would be noise in every log line.
        assert_eq!(
            query_word("@@rules_jvm_external++maven+maven//:bar"),
            "@@rules_jvm_external++maven+maven//:bar"
        );
    }

    #[test]
    fn parses_load_errors_from_coloured_stderr() {
        let e = parse_load_errors(APOLLO_STDERR);
        assert_eq!(e.count, 3);
        assert_eq!(
            e.packages,
            vec![
                "modules/planning/traffic_rules/crosswalk".to_string(),
                // The root package is a real package name, not "no package".
                String::new(),
                "third_party/bazel".to_string(),
            ]
        );
    }

    #[test]
    fn ignores_lines_that_are_not_load_errors() {
        for line in [
            "INFO: Elapsed time: 3.2s",
            "WARNING: --keep_going specified, ignoring errors.",
            "ERROR: Skipping '//foo:bar': no such target",
            "ERROR: Analysis of target '//x:y' failed",
            "",
        ] {
            assert_eq!(parse_load_error_line(line), None, "{line:?}");
        }
    }

    #[test]
    fn counts_an_external_repository_that_would_not_load() {
        assert_eq!(
            parse_load_error_line(
                "\u{1b}[31mERROR: \u{1b}[0mno such package '@@local_config_cuda//cuda': \
                 Cuda Configuration Error"
            )
            .as_deref(),
            Some("@@local_config_cuda//cuda")
        );
    }

    /// Every load failure a package query (`set(//p:* ...)`, the dirty set's
    /// re-query) reports comes as `Skipping '<pattern>': <why>`, and a
    /// whole-package pattern names its package whatever the reason names.
    /// The first two lines are verbatim from Bazel 8.7.0 on xla at
    /// `e5dd39c1c9dc` (only the worktree and output base paths shortened to
    /// `/ws` and `/ob`), the next two from a two-package workspace whose
    /// `a/BUILD` loads from a repository rule that fails and whose `c/BUILD`
    /// loads from a repository that does not exist.
    #[test]
    fn a_skipped_package_pattern_names_its_package() {
        for (line, want) in [
            (
                "ERROR: Skipping '//xla/mlir_hlo:*': error loading package 'xla/mlir_hlo': \
                 Encountered error while reading extension file 'mlir/tblgen.bzl': no such \
                 package '@@+llvm_extension+llvm-project//mlir': no such package \
                 '@@+third_party_ext+llvm-raw//utils/bazel': Error applying patch \
                 /ws/third_party/llvm/platforms_config.patch: in patch applied to \
                 /ob/external/+third_party_ext+llvm-raw/utils/bazel/llvm-project-overlay/llvm/\
                 config.bzl: could not apply patch due to CONTENT_DOES_NOT_MATCH_TARGET, error \
                 applying change near line 49",
                "xla/mlir_hlo",
            ),
            (
                "ERROR: Skipping '//xla/mlir_hlo/tests:*': error loading package \
                 'xla/mlir_hlo/tests': Encountered error while reading extension file \
                 'llvm/lit_test.bzl': no such package '@@+llvm_extension+llvm-project//llvm': \
                 no such package '@@+third_party_ext+llvm-raw//utils/bazel': Error applying \
                 patch /ws/third_party/llvm/platforms_config.patch",
                "xla/mlir_hlo/tests",
            ),
            (
                "ERROR: Skipping '//a:*': no such package '@@+_repo_rules+broken//': patch \
                 does not apply (simulated)",
                "a",
            ),
            (
                "ERROR: Skipping '//c:*': error loading package 'c': Unable to find package \
                 for @@[unknown repo 'nope' requested from @@]//:x.bzl: The repository \
                 '@@[unknown repo 'nope' requested from @@]' could not be resolved: No \
                 repository visible as '@nope' from main repository.",
                "c",
            ),
            (
                "\u{1b}[31m\u{1b}[1mERROR: \u{1b}[0mSkipping '//pkg:*': no such package \
                 '@@+_repo_rules+brk//': patch failed to apply",
                "pkg",
            ),
            (
                "ERROR: Skipping '//:*': no such package '@@ext//': boom",
                "",
            ),
            (
                "ERROR: Skipping '@tsl//tsl/platform:*': no such package '@@foo//': boom",
                "@tsl//tsl/platform",
            ),
        ] {
            assert_eq!(
                parse_load_error_line(line).as_deref(),
                Some(want),
                "{line:?}"
            );
        }
    }

    /// A pattern that is not one whole package names no package of its own,
    /// so the line is read by its reason, exactly as the same reason on a
    /// line of its own would be: a repository that failed is still a load
    /// error, a missing target or an empty walk is not one.
    #[test]
    fn a_skipped_pattern_that_is_not_a_package_is_read_by_its_reason() {
        for (line, want) in [
            (
                "ERROR: Skipping '//...:*': no such package '@@ext//': boom",
                Some("@@ext//"),
            ),
            (
                "ERROR: Skipping '//foo:bar': error loading package 'foo': boom",
                Some("foo"),
            ),
            ("ERROR: Skipping '//foo:bar': no such target", None),
            (
                "ERROR: Skipping '//a/...': no targets found beneath 'a'",
                None,
            ),
        ] {
            assert_eq!(parse_load_error_line(line).as_deref(), want, "{line:?}");
        }
    }

    /// A skipped pattern whose package itself is not there was not a load
    /// failure: its directory has no `BUILD` file, which is a deletion.
    #[test]
    fn a_skipped_package_with_no_build_file_is_not_a_load_error() {
        assert_eq!(
            parse_load_error_line(
                "ERROR: Skipping '//gone:*': no such package 'gone': BUILD file not found in \
                 any of the following directories. Add a BUILD file to a directory to mark it \
                 as a package."
            ),
            None
        );
    }

    /// An external package that is gone is skipped with Bazel's canonical
    /// spelling in the reason and the apparent one in the pattern; it is a
    /// deletion all the same. One that failed to load, or a different
    /// package or repository in the reason, is still a load error.
    #[test]
    fn a_skipped_external_package_that_is_gone_is_not_a_load_error() {
        for canonical in ["@@tsl", "@@tsl+", "@@+_repo_rules+tsl", "@@_main~ext~tsl"] {
            let line = format!(
                "ERROR: Skipping '@tsl//tsl/gone:*': no such package '{canonical}//tsl/gone': \
                 BUILD file not found in any of the following directories."
            );
            assert_eq!(parse_load_error_line(&line), None, "{canonical}");
        }
        assert_eq!(
            parse_load_error_line(
                "ERROR: Skipping '@tsl//tsl/gone:*': no such package '@@+ext+other_tsl//': \
                 patch does not apply"
            )
            .as_deref(),
            Some("@tsl//tsl/gone")
        );
        assert_eq!(
            parse_load_error_line(
                "ERROR: Skipping '@tsl//tsl/x:*': no such package '@@tsl//tsl/gone': \
                 BUILD file not found"
            )
            .as_deref(),
            Some("@tsl//tsl/x")
        );
        assert_eq!(
            parse_load_error_line(
                "ERROR: Skipping '@tsl//tsl/gone:*': no such package '@@notsl//tsl/gone': \
                 BUILD file not found"
            )
            .as_deref(),
            Some("@tsl//tsl/gone")
        );
    }

    /// The line envoy's own `--deleted_packages` produces when a dirty package
    /// under `mobile/test/common/integration` is asked for by name.
    const DELETED_LINE: &str = "ERROR: Skipping '//mobile/test/common/integration:*': \
         no such package 'mobile/test/common/integration': Package is considered deleted \
         due to --deleted_packages";

    #[test]
    fn a_declared_deleted_package_is_not_a_load_error() {
        // It carries a `no such package '...'` clause, so before this it was
        // counted as one — and a run that touched such a package published an
        // index flagged partial with no error lines to show for it.
        assert_eq!(parse_load_error_line(DELETED_LINE), None);
        assert_eq!(
            parse_deleted_package_line(DELETED_LINE).as_deref(),
            Some("mobile/test/common/integration")
        );
        let f = parse_stderr_facts(&format!("{DELETED_LINE}\nINFO: Elapsed time: 3.2s\n"));
        assert!(f.load_errors.is_empty());
        assert_eq!(f.deleted_packages, ["mobile/test/common/integration"]);
    }

    #[test]
    fn a_load_failure_next_to_a_deleted_package_still_counts() {
        let f = parse_stderr_facts(&format!(
            "{DELETED_LINE}\nERROR: error loading package 'modules/planning': boom\n"
        ));
        assert_eq!(f.load_errors.count, 1);
        assert_eq!(f.load_errors.packages, ["modules/planning".to_string()]);
        assert_eq!(f.deleted_packages.len(), 1);
    }

    #[test]
    fn keeps_only_the_first_n_package_names_but_counts_them_all() {
        let mut s = String::new();
        for i in 0..(MAX_ERROR_PACKAGES + 5) {
            s.push_str(&format!("ERROR: error loading package 'p{i}': boom\n"));
        }
        let e = parse_load_errors(&s);
        assert_eq!(e.count as usize, MAX_ERROR_PACKAGES + 5);
        assert_eq!(e.packages.len(), MAX_ERROR_PACKAGES);
        assert_eq!(e.packages[0], "p0");
    }

    /// Under bzlmod `bazel query` prints canonical repository names, and the
    /// probe that reads only `@tsl//` reads a repository that loaded fine as
    /// one with no packages at all.
    #[test]
    fn a_probe_reads_both_spellings_of_a_repository_name() {
        assert_eq!(
            package_of_repo("@tsl//tsl/platform", "tsl"),
            Some("tsl/platform".into())
        );
        assert_eq!(
            package_of_repo("@@tsl+//tsl/platform", "tsl"),
            Some("tsl/platform".into())
        );
        assert_eq!(
            package_of_repo("@@tsl~//tsl/platform", "tsl"),
            Some("tsl/platform".into())
        );
        assert_eq!(
            package_of_repo("@@+xla_extension+tsl//tsl/platform", "tsl"),
            Some("tsl/platform".into())
        );
        // The repository root is a package, and it is not the empty answer.
        assert_eq!(package_of_repo("@@tsl+//", "tsl"), Some(String::new()));
        // A label, as the consumer query prints one, names its package.
        assert_eq!(
            package_of_repo("@envoy_api//envoy/admin/v3:pkg", "envoy_api"),
            Some("envoy/admin/v3".into())
        );
        assert_eq!(package_of_repo("@@tsl+//:root", "tsl"), Some(String::new()));
        // Another repository's packages are not this one's.
        assert_eq!(package_of_repo("@@tsl_other+//pkg", "tsl"), None);
        assert_eq!(package_of_repo("@other//pkg", "tsl"), None);
        assert_eq!(package_of_repo("not a label", "tsl"), None);
    }

    #[test]
    fn finds_the_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("MODULE.bazel"), "").unwrap();
        let deep = root.join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(
            find_workspace_root(&deep).unwrap().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn reports_a_missing_workspace() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_workspace_root(dir.path()).is_none());
    }

    #[test]
    fn parses_rbuildfiles_output() {
        let out = "//lib/core:BUILD\n//app:BUILD.bazel\n//:BUILD\n//weird:thing\ngarbage\n";
        let r = parse_buildfile_labels(out);
        assert_eq!(r.packages, vec!["", "app", "lib/core"]);
        assert!(r.workspace_level.is_empty());
        assert!(!r.is_empty());
    }

    #[test]
    fn separates_workspace_level_loaders_from_packages() {
        // bzlmod, 9.2.0 / 8.5.1 / 7.4.1: a module-extension `.bzl` names only
        // the external repository it defines.
        let r = parse_buildfile_labels("@my_repo//:BUILD\n");
        assert!(r.packages.is_empty());
        assert_eq!(r.workspace_level, vec!["@my_repo//:BUILD"]);
        assert!(!r.is_empty());

        // WORKSPACE mode, 7.4.1 / 8.5.1: the pseudo-package plus everything.
        let r = parse_buildfile_labels(
            "//external:WORKSPACE\n@my_repo//:BUILD\n//app:BUILD\n//bazel:BUILD\n\
             @bazel_tools//tools/genrule:BUILD\n",
        );
        assert_eq!(r.packages, vec!["app", "bazel"]);
        assert_eq!(
            r.workspace_level,
            vec![
                "//external:WORKSPACE",
                "@bazel_tools//tools/genrule:BUILD",
                "@my_repo//:BUILD"
            ]
        );

        // The workspace files themselves, and canonical `@@` repo names.
        let r =
            parse_buildfile_labels("//:MODULE.bazel\n//:WORKSPACE.bazel\n@@rules_cc+//:BUILD\n");
        assert!(r.packages.is_empty());
        assert_eq!(r.workspace_level.len(), 3);

        // A dead `.bzl`: nothing at all.
        assert!(parse_buildfile_labels("").is_empty());
        assert!(parse_buildfile_labels("INFO: Empty results\n").is_empty());
    }
}

/// What one `bazel query '@R//...' --output=package` answered.
///
/// `packages` are repository-relative package names, `""` for the repository
/// root (which prints as `@R//`). `load_error` says a package of this
/// repository failed to load, which makes the list an undercount and is why
/// a claim is never confirmed from one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoProbe {
    pub repo: String,
    pub packages: Vec<String>,
    pub load_error: bool,
}

#[cfg(test)]
mod consumer_tests {
    use super::{
        apparent_of_canonical, consumed_expr, label_repo_token, leaf_expr, parse_leaf_reach,
    };
    use std::collections::BTreeSet;

    fn set(ps: &[&str]) -> BTreeSet<String> {
        ps.iter().map(|p| p.to_string()).collect()
    }

    /// The table's packages are the bridge's transitive closure inside the
    /// repository. The depth-1 query recorded 36 `@envoy_api` packages at
    /// envoy's pin against a closure of 48, and an edit to one of the twelve
    /// (`envoy/type/metadata/v3`, under `envoy/config/route/v3`) skipped the
    /// consumer.
    #[test]
    fn the_consumed_packages_are_asked_for_transitively() {
        let bridge = vec![
            "@@grpc+//src/core:grpc_xds_client".to_string(),
            "@@grpc+//:grpc++".to_string(),
        ];
        assert_eq!(
            consumed_expr("envoy_api", &bridge),
            "deps(set(\"@@grpc+//src/core:grpc_xds_client\" \"@@grpc+//:grpc++\")) \
             intersect @envoy_api//...:*"
        );
        assert!(!consumed_expr("envoy_api", &bridge).contains(", 1)"));
    }

    /// A graph as Bazel 8.7 prints it for the leaves' closure, cut down: one
    /// leaf with a path in, through two hops of `@grpc` and a `select` edge,
    /// into one API package that depends on a second, and past a main-repo
    /// rule and an `@abseil-cpp` one on the way; one leaf with no path in,
    /// whose closure runs through a main-repo rule that could get one after
    /// a `BUILD` edit (quiche's shape, through
    /// `@envoy//source/common/quic/platform`); and one leaf Bazel never
    /// printed.
    const GRAPH: &str = r#"digraph mygraph {
  node [shape=box];
  "@grpc//src/proto/grpc/health/v1:health_cc_proto"
  "@grpc//src/proto/grpc/health/v1:health_cc_proto" -> "//source/common/quic/platform:quic_platform_impl_lib"
  "//source/common/quic/platform:quic_platform_impl_lib" -> "//source/common/common:assert_lib"
  "@grpc//:grpc++"
  "@grpc//:grpc++" -> "@grpc//:grpc++_xds_client"
  [label="//conditions:default"];
  "@grpc//:grpc++_xds_client" -> "@grpc//src/core:grpc_xds_client"
  "@grpc//:grpc++_xds_client" -> "@abseil-cpp//absl/strings:strings"
  "@grpc//src/core:grpc_xds_client" -> "@@//:config"
  "@grpc//src/core:grpc_xds_client" -> "@envoy_api//envoy/extensions/filters/http/fault/v3:pkg"
  "@envoy_api//envoy/extensions/filters/http/fault/v3:pkg" -> "@envoy_api//envoy/type/metadata/v3:pkg"
  "@envoy_api//envoy/type/metadata/v3:pkg" -> "@envoy_api//envoy/type/metadata/v3:metadata.proto"
  "@envoy_api//envoy/type/metadata/v3:metadata.proto"
}
"#;

    #[test]
    fn each_leaf_is_walked_on_its_own_through_the_graph() {
        let leaves = vec![
            "@grpc//:grpc++".to_string(),
            "@grpc//src/proto/grpc/health/v1:health_cc_proto".to_string(),
            "@grpc//src/compiler:grpc_cpp_plugin".to_string(),
        ];
        let r = parse_leaf_reach(GRAPH, "envoy_api", &leaves);
        // Transitive, and packages rather than labels.
        assert_eq!(
            r["@grpc//:grpc++"].packages,
            set(&[
                "envoy/extensions/filters/http/fault/v3",
                "envoy/type/metadata/v3"
            ])
        );
        // What it passes through outside the repository: its own repository,
        // another, and the main repository's root package.
        assert_eq!(
            r["@grpc//:grpc++"].via,
            set(&["//", "@abseil-cpp", "@grpc"])
        );
        // Resolved, no path: known to reach nothing, and known to do so
        // through two main-repo packages whose `BUILD` files could change it.
        let health = &r["@grpc//src/proto/grpc/health/v1:health_cc_proto"];
        assert_eq!(health.packages, set(&[]));
        assert_eq!(
            health.via,
            set(&[
                "//source/common/common",
                "//source/common/quic/platform",
                "@grpc"
            ])
        );
        // Never printed: not known, and so not in the map at all.
        assert!(
            !r.contains_key("@grpc//src/compiler:grpc_cpp_plugin"),
            "{r:?}"
        );
        assert_eq!(r.len(), 2);
    }

    /// The query is the leaves' whole closure less the source files outside
    /// the repository, not the paths into it: the rules that could carry a
    /// new path after a `BUILD` edit are on the closure and not on any path.
    #[test]
    fn the_leaf_query_is_the_closure_not_the_paths() {
        let e = leaf_expr("envoy_api", &["@grpc//:grpc++".to_string()]);
        assert_eq!(
            e,
            "let d = deps(set(\"@grpc//:grpc++\")) in \
             $d except (kind(\"source file\", $d) except @envoy_api//...:*)"
        );
        assert!(!e.contains("allpaths"));
    }

    /// Bazel may print canonical names where the index spells apparent ones;
    /// the answer is keyed by the caller's spelling either way, and the
    /// repository's packages are read in either spelling too, Bazel 7's `~`
    /// separator included.
    #[test]
    fn a_leaf_is_matched_whichever_way_the_graph_spells_its_repository() {
        let plus = GRAPH
            .replace("@grpc//", "@@grpc+//")
            .replace("@envoy_api//", "@@envoy_api+//");
        let tilde = GRAPH
            .replace("@grpc//", "@@grpc~//")
            .replace("@envoy_api//", "@@envoy_api~//");
        for g in [GRAPH.to_string(), plus, tilde] {
            for leaf in ["@grpc//:grpc++", "@@grpc+//:grpc++", "@@grpc~//:grpc++"] {
                let r = parse_leaf_reach(&g, "envoy_api", &[leaf.to_string()]);
                assert_eq!(r.len(), 1, "{leaf}: {r:?}");
                assert_eq!(r[leaf].packages.len(), 2, "{leaf}: {r:?}");
            }
        }
        // Another repository's packages are not this one's.
        let r = parse_leaf_reach(GRAPH, "envoy_api_other", &["@grpc//:grpc++".to_string()]);
        assert_eq!(r["@grpc//:grpc++"].packages, set(&[]));
        // Nothing printed, nothing known.
        assert!(parse_leaf_reach("", "envoy_api", &["@grpc//:grpc++".to_string()]).is_empty());
    }

    /// Two nodes Bazel printed under two spellings of one label are both the
    /// leaf: the walk starts from each, so an edge hung off either counts.
    #[test]
    fn a_label_printed_under_two_spellings_is_walked_from_both() {
        let g = r#"digraph mygraph {
  "@grpc//:grpc++" -> "@grpc//:a"
  "@@grpc+//:grpc++" -> "@envoy_api//envoy/b:pkg"
}
"#;
        let r = parse_leaf_reach(g, "envoy_api", &["@grpc//:grpc++".to_string()]);
        assert_eq!(r["@grpc//:grpc++"].packages, set(&["envoy/b"]));
    }

    #[test]
    fn a_labels_repository_token_is_read_in_either_spelling() {
        assert_eq!(label_repo_token("@@grpc+//src/core:x"), Some("grpc+"));
        assert_eq!(label_repo_token("@grpc//:grpc"), Some("grpc"));
        assert_eq!(label_repo_token("//a:b"), None);
        assert_eq!(label_repo_token("@@//a:b"), None);
    }

    #[test]
    fn a_canonical_token_gives_back_its_apparent_name() {
        assert_eq!(apparent_of_canonical("grpc+"), "grpc");
        assert_eq!(apparent_of_canonical("grpc"), "grpc");
        assert_eq!(apparent_of_canonical("+xla_extension+tsl"), "tsl");
        assert_eq!(
            apparent_of_canonical("envoy_toolshed++llvm_ext+llvm"),
            "llvm"
        );
    }
}

#[cfg(test)]
mod round_trip_tests {
    use super::{
        closure_loads_via, inside_label, inside_patterns, loads_expr, package_lines,
        parse_closure_repos, parse_round_trips, LabelGraph,
    };
    use std::collections::BTreeSet;

    /// `loadfiles` over the closure less the universe's packages, which carry
    /// their own loads; with none of them in it, over the whole closure.
    #[test]
    fn the_loads_query_leaves_out_the_universes_packages() {
        let leaves = ["@bazel_tools//tools/test:collect_coverage".to_string()];
        assert_eq!(
            loads_expr(&leaves, &BTreeSet::new()),
            "loadfiles(deps(set(\"@bazel_tools//tools/test:collect_coverage\")))"
        );
        assert_eq!(
            loads_expr(&leaves, &set(&["//shell/runfiles:*", "//shell:*"])),
            "loadfiles(deps(set(\"@bazel_tools//tools/test:collect_coverage\")) \
             except (//shell/runfiles:* + //shell:*))"
        );
    }

    /// `bazel query 'loadfiles(deps(@bazel_tools//tools/test:collect_coverage))'`
    /// on rules_shell at `b1eed28` (Bazel 9.1.1), cut down, plus an indexed
    /// repository's file: the root module's `.bzl` files are the ones the
    /// collector's rule class comes from.
    #[test]
    fn the_closures_loads_become_via_entries() {
        const LOADS: &str = "//shell:sh_binary.bzl\n\
            //shell/private:sh_binary.bzl\n\
            //shell/private:sh_executable.bzl\n\
            @@//:defs.bzl\n\
            @bazel_skylib//:bzl_library.bzl\n\
            @@bazel_tools//tools/test:BUILD\n\
            @@envoy_api+//bazel:api_build_system.bzl\n";
        assert_eq!(
            closure_loads_via(LOADS, &["envoy_api".to_string()]),
            set(&[
                "//:defs.bzl",
                "//shell/private:sh_binary.bzl",
                "//shell/private:sh_executable.bzl",
                "//shell:sh_binary.bzl",
                "@envoy_api",
            ])
        );
        assert!(closure_loads_via("@rules_cc//cc:defs.bzl\n", &[]).is_empty());
    }

    fn set(ps: &[&str]) -> BTreeSet<String> {
        ps.iter().map(|p| p.to_string()).collect()
    }

    /// `bazel query --output=package` over the leaves' closure on
    /// rules_shell at `b1eed28`, cut down: the main repository's packages
    /// print with no repository at all.
    const PACKAGES: &str = "@bazel_tools//tools/test\n@@rules_java+//toolchains\nshell\nshell/runfiles\n@@envoy_api+//envoy/type\n";

    #[test]
    fn the_closures_inside_packages_become_patterns() {
        assert_eq!(
            inside_patterns(PACKAGES, &[]),
            set(&["//shell/runfiles:*", "//shell:*"])
        );
        // An indexed repository's packages are inside too, spelled as Bazel
        // printed them; the root package is `//:*`.
        assert_eq!(
            inside_patterns(&format!("{PACKAGES}\n"), &["envoy_api".to_string()]),
            set(&[
                "//:*",
                "//shell/runfiles:*",
                "//shell:*",
                "@@envoy_api+//envoy/type:*"
            ])
        );
        assert!(inside_patterns("@bazel_tools//tools/test\n", &[]).is_empty());
    }

    /// Verbatim from rules_shell at `b1eed28` (Bazel 9.1.1), plus a second
    /// leaf whose only way in runs through the first, and an external hop.
    const GRAPH: &str = r#"digraph mygraph {
  node [shape=box];
  "@bazel_tools//tools/test:collect_coverage"
  "@bazel_tools//tools/test:collect_coverage" -> "//shell/runfiles:runfiles"
  "@bazel_tools//tools/test:collect_coverage" -> "//shell:toolchain_type"
  "//shell:toolchain_type"
  "//shell/runfiles:runfiles"
  "//shell/runfiles:runfiles" -> "//shell/runfiles:runfiles_impl"
  "//shell/runfiles:runfiles_impl"
  "//shell/runfiles:runfiles_impl" -> "//shell/runfiles:runfiles_at_legacy_location"
  "//shell/runfiles:runfiles_at_legacy_location"
  "//shell/runfiles:runfiles_at_legacy_location" -> "//shell/runfiles:runfiles.bash"
  "//shell/runfiles:runfiles.bash"
  "@bazel_tools//tools/test:test_setup" -> "@@rules_x+//:hop"
  [label="//conditions:default"];
  "@@rules_x+//:hop" -> "@bazel_tools//tools/test:collect_coverage"
  "@@rules_x+//:hop" -> "@@envoy_api+//envoy/type:pkg"
}
"#;

    /// The walk stops at the first node inside: the runfiles library's own
    /// deps are the index's rows, not the leaf's. A path through another
    /// external node counts, and ends where it enters.
    #[test]
    fn each_leaf_gets_the_first_inside_node_of_every_path() {
        let leaves = vec![
            "@bazel_tools//tools/test:collect_coverage".to_string(),
            "@bazel_tools//tools/test:test_setup".to_string(),
            "@platforms//os:linux".to_string(),
        ];
        let g = LabelGraph::parse(GRAPH);
        let r = parse_round_trips(&g, &[], &leaves);
        assert_eq!(
            r["@bazel_tools//tools/test:collect_coverage"],
            set(&["//shell/runfiles:runfiles", "//shell:toolchain_type"])
        );
        assert_eq!(
            r["@bazel_tools//tools/test:test_setup"],
            set(&["//shell/runfiles:runfiles", "//shell:toolchain_type"])
        );
        // Reaches nothing (never printed): absent.
        assert_eq!(r.len(), 2, "{r:?}");
        // An indexed repository is inside, under the name the index knows.
        let r = parse_round_trips(&g, &["envoy_api".to_string()], &leaves[1..2]);
        assert_eq!(
            r["@bazel_tools//tools/test:test_setup"],
            set(&[
                "//shell/runfiles:runfiles",
                "//shell:toolchain_type",
                "@envoy_api//envoy/type:pkg"
            ])
        );
    }

    /// The graph's nodes give the packages `--output=package` printed, and
    /// so the same patterns: the closure is asked once, as a graph.
    #[test]
    fn the_graphs_packages_become_the_same_patterns() {
        let g = LabelGraph::parse(GRAPH);
        let lines = package_lines(&g);
        assert_eq!(
            inside_patterns(&lines, &[]),
            set(&["//shell/runfiles:*", "//shell:*"])
        );
        assert_eq!(
            inside_patterns(&lines, &["envoy_api".to_string()]),
            set(&[
                "//shell/runfiles:*",
                "//shell:*",
                "@@envoy_api+//envoy/type:*"
            ])
        );
    }

    /// envoy's shape at `eea2ba5e2911`, cut down: `@grpc//:grpc++` runs
    /// through `@c-ares` (the only way the rules above `grpc++` reach it),
    /// and through `@envoy_api`, which is inside when the index holds it. Each leaf's
    /// closure is its own walk; a leaf the graph never printed is left out.
    #[test]
    fn each_leaf_records_the_repositories_its_closure_runs_through() {
        const ENVOY: &str = r#"digraph mygraph {
  node [shape=box];
  "@grpc//:grpc++" -> "@grpc//:grpc"
  "@grpc//:grpc" -> "@c-ares//:ares"
  "@grpc//:grpc" -> "@envoy_api//envoy/service:pkg"
  "@envoy_api//envoy/service:pkg" -> "@com_google_protobuf//:protobuf"
  "@c-ares//:ares" -> "@c-ares//:ares.h"
  "@grpc//:grpc" -> "//bazel:grpc_config"
  "//bazel:grpc_config" -> "@@zlib//:zlib"
  "@com_google_absl//absl/strings:strings"
}
"#;
        let g = LabelGraph::parse(ENVOY);
        let leaves = vec![
            "@grpc//:grpc++".to_string(),
            "@com_google_absl//absl/strings:strings".to_string(),
            "@never//:printed".to_string(),
        ];
        let c = parse_closure_repos(&g, &[], &leaves);
        // The walk stops at the main repository's node: `@zlib` beyond it
        // is a leaf of the index's own, seeded on its own when it moves.
        assert_eq!(
            c["@grpc//:grpc++"],
            set(&["@c-ares", "@envoy_api", "@com_google_protobuf", "@grpc"])
        );
        assert_eq!(
            c["@com_google_absl//absl/strings:strings"],
            set(&["@com_google_absl"])
        );
        assert_eq!(c.len(), 2, "{c:?}");
        // With `@envoy_api` indexed, the walk stops there too.
        let c = parse_closure_repos(&g, &["envoy_api".to_string()], &leaves[..1]);
        assert_eq!(c["@grpc//:grpc++"], set(&["@c-ares", "@grpc"]));
    }

    #[test]
    fn an_inside_label_is_spelled_as_the_index_spells_it() {
        assert_eq!(inside_label("@@//a:b", &[]), Some("//a:b".to_string()));
        assert_eq!(inside_label("@//a:b", &[]), Some("//a:b".to_string()));
        assert_eq!(inside_label("//a:b", &[]), Some("//a:b".to_string()));
        assert_eq!(inside_label("@@tsl+//t:x", &[]), None);
        let tsl = ["tsl".to_string()];
        assert_eq!(
            inside_label("@@+ext+tsl//t:x", &tsl),
            Some("@tsl//t:x".to_string())
        );
        assert_eq!(
            inside_label("@tsl//t:x", &tsl),
            Some("@tsl//t:x".to_string())
        );
        assert_eq!(inside_label("@@grpc+//:g", &tsl), None);
    }
}
