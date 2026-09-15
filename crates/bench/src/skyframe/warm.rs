//! `warm-query`: no index at all, just `bazel query rdeps` on a live server.
//!
//! The cheapest thing that could possibly work. There is no index, no dump and
//! no state on disk: `prepare` warms a Bazel server at the parent with `bazel
//! query //...`, and `answer` asks that same server, now at head, the oracle's
//! own question:
//!
//! ```text
//! kind(rule, rdeps(//..., set(//pkg:a.cc //pkg:defs.bzl ...)))
//! ```
//!
//! What makes this interesting is not the query, which is obvious, but
//! Skyframe. The server already holds the loaded graph from `prepare`; the
//! checkout then moves to head under it, and Bazel's own incremental
//! invalidation is supposed to re-load only the packages the change touched.
//! So the number that decides whether the design is viable is not only the
//! answer wall but **how many packages the second query re-loaded**, which is
//! the true measure of change-proportionality. Both are recorded.
//!
//! Every other strategy has to beat this one on wall time, and this one has to
//! agree with the oracle on content, since it *is* the oracle's query. Any
//! disagreement is a harness or label-mapping bug, not a finding.
//!
//! ## Naming the changed paths
//!
//! The label mapping is [`crate::oracles::rdeps`]'s, reused rather than
//! re-derived: [`rdeps::source_label`] for the label, [`rdeps::batches`] for
//! the batching, [`rdeps::rdeps_expression`] for the expression. The one thing
//! this strategy cannot borrow is the oracle's package list, which comes from
//! an index, and having no index is the whole point. It reads the packages off
//! the head worktree instead: an ancestor directory holding a `BUILD` or
//! `BUILD.bazel` file is a package. That is the rule Bazel applies, and the
//! only way the two can diverge is a package `//...` refuses to load, which
//! costs a root that resolves to nothing rather than a wrong answer.
//!
//! ## Deleted paths
//!
//! A path that is gone at head cannot be named as a source label: Bazel
//! rejects `//pkg:deleted.cc`, and under batching one rejected label costs its
//! neighbours their answer. The choice made here, in order:
//!
//! 1. If some ancestor of the deleted path is still a package at head, seed
//!    `//pkg:*` instead. Every target in the package that held the file is a
//!    superset of the file's own reverse dependencies, so this is sound and
//!    wider, which is the safe direction.
//! 2. Otherwise the package itself is gone. If the deleted path is a `.bzl` or
//!    a `BUILD` file, `rbuildfiles(<path>)` names the packages that loaded it
//!    (Bazel answers that for a file that no longer exists, since it is a
//!    question about the graph the server holds), and those packages are
//!    seeded as `//pkg:*`.
//! 3. Otherwise the path is dropped, and counted in `answer.unnamed_deletes`.
//!
//! Step 3 is where this strategy is knowingly weaker than an index: cambium
//! answers a deleted non-BUILD file in a deleted package from its index at the
//! parent, and a live server at head has nothing left to be asked about it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

use anyhow::{Context as _, Result};
use cambium_bazel::Bazel;
use cambium_classify::PackageSet;

use crate::oracles::rdeps;

use super::{Answer, Prepared, StepContext, Strategy};

/// The variants worth one run each. Each changes one flag or one step of the
/// query and nothing else, so two variants are comparable to each other and to
/// the base row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// `--universe_scope=//...`, which puts the query in Sky Query mode
    /// instead of evaluating `//...` inside the `rdeps` expression.
    pub universe_scope: bool,
    /// `--infer_universe_scope`, which lets Bazel pick the scope from the
    /// expression's own target patterns.
    pub infer_universe_scope: bool,
    /// Drop `--keep_going`. The subject cannot load the whole of its `//...`
    /// on macOS, so this is expected to fail rather than to be fast; it is
    /// here to record how it fails.
    pub no_keep_going: bool,
    /// Ask for ordered output, which is Bazel's default and costs a sort.
    pub ordered: bool,
    /// Resolve the changed `.bzl` and `BUILD` files through `rbuildfiles`
    /// first and seed the packages it names, instead of the files themselves.
    pub rbuildfiles_first: bool,
    /// `bazel shutdown` before `answer`, so the answer is measured on a cold
    /// server. This is the honest other half of the number: it says whether
    /// the design fits CI, where no server is waiting, or dev machines only.
    pub cold: bool,
}

impl Options {
    /// Parses the suffix of a strategy name: `warm-query-cold` and so on.
    /// `None` if the name is not a `warm-query` variant at all.
    pub fn from_name(name: &str) -> Option<Self> {
        let rest = name.strip_prefix("warm-query")?;
        let mut o = Self::default();
        for part in rest.split('-').filter(|p| !p.is_empty()) {
            match part {
                "cold" => o.cold = true,
                "scope" => o.universe_scope = true,
                "infer" => o.infer_universe_scope = true,
                "nokeepgoing" => o.no_keep_going = true,
                "ordered" => o.ordered = true,
                "rbuildfiles" => o.rbuildfiles_first = true,
                _ => return None,
            }
        }
        Some(o)
    }

    /// The query flags this variant passes, beyond `--output=label`.
    fn query_flags(&self) -> Vec<String> {
        let mut flags = Vec::new();
        if !self.ordered {
            flags.push("--order_output=no".to_string());
        }
        if !self.no_keep_going {
            flags.push("--keep_going".to_string());
        }
        if self.universe_scope {
            flags.push("--universe_scope=//...".to_string());
        }
        if self.infer_universe_scope {
            flags.push("--infer_universe_scope".to_string());
        }
        // No `--noshow_progress`: the progress line is where "N packages
        // loaded" comes from, and that number is the point of the measurement.
        flags.push("--ui_event_filters=-info".to_string());
        flags
    }
}

/// `warm-query`, in one of its [`Options`] variants.
pub struct WarmQueryStrategy {
    name: String,
    opts: Options,
    /// The server pid `prepare` warmed, to compare with the one that answered.
    prepared_pid: Option<String>,
}

impl WarmQueryStrategy {
    pub fn new(name: &str, opts: Options) -> Self {
        Self {
            name: name.to_string(),
            opts,
            prepared_pid: None,
        }
    }
}

/// Exit code Bazel uses for "finished, with errors" under `--keep_going`.
const PARTIAL: i32 = 3;

/// What one `bazel query` invocation answered.
struct QueryRun {
    labels: BTreeSet<String>,
    partial: bool,
    /// The largest "N packages loaded" the progress lines reported.
    packages_loaded: u64,
}

/// Runs one query on `bazel`'s server and reads the packages it re-loaded off
/// the progress lines.
fn run_query(bazel: &Bazel, expr: &str, opts: &Options) -> Result<QueryRun> {
    let what = crate::tools::tail(expr, 1);
    let out = bazel
        .command("query")
        .arg(expr)
        .arg("--output=label")
        .args(opts.query_flags())
        .output()
        .with_context(|| format!("spawning bazel query {what}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let partial = out.status.code() == Some(PARTIAL);
    anyhow::ensure!(
        out.status.success() || partial,
        "bazel query {what} failed with {}\n{}",
        out.status,
        crate::tools::tail(&stderr, 20)
    );
    Ok(QueryRun {
        labels: crate::tools::bazel_query::parse_labels(&String::from_utf8_lossy(&out.stdout)),
        partial,
        packages_loaded: packages_loaded(&stderr),
    })
}

/// The packages Bazel re-loaded, off the `Loading: N packages loaded` progress
/// lines Bazel writes to stderr.
///
/// The lines are rewritten in place with carriage returns and the count only
/// grows, so the largest one is the total for the invocation. A server that
/// re-loaded nothing prints no such line at all and the answer is 0, which is
/// the change-proportional ideal rather than a missing measurement.
pub fn packages_loaded(stderr: &str) -> u64 {
    let mut max = 0u64;
    for chunk in stderr.split(['\r', '\n']) {
        let Some((head, _)) = chunk.split_once(" packages loaded") else {
            continue;
        };
        let digits: String = head
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect::<Vec<char>>()
            .into_iter()
            .rev()
            .collect();
        if let Ok(n) = digits.parse::<u64>() {
            max = max.max(n);
        }
    }
    max
}

/// `bazel info server_pid`, or `None` if it cannot be read. It only says in
/// the result file whether the server that answered is the server that was
/// warmed; nothing depends on it.
fn server_pid(bazel: &Bazel) -> Option<String> {
    let out = bazel.command("info").arg("server_pid").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let pid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!pid.is_empty()).then_some(pid)
}

/// The packages of the head worktree, read off disk rather than off an index:
/// every ancestor directory of a changed path that holds a `BUILD` or
/// `BUILD.bazel` file.
///
/// Only the changed paths' ancestors are probed, so this is a few dozen
/// `stat` calls, not a repository walk.
pub fn packages_from_disk<'a>(
    worktree: &Path,
    paths: impl IntoIterator<Item = &'a str>,
) -> PackageSet {
    let mut found: BTreeSet<String> = BTreeSet::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for path in paths {
        let mut dir = match path.rfind('/') {
            Some(i) => &path[..i],
            None => "",
        };
        loop {
            if !seen.insert(dir.to_string()) {
                break;
            }
            if is_package(worktree, dir) {
                found.insert(dir.to_string());
            }
            match dir.rfind('/') {
                Some(i) => dir = &dir[..i],
                None if dir.is_empty() => break,
                None => dir = "",
            }
        }
    }
    PackageSet::new(found)
}

fn is_package(worktree: &Path, dir: &str) -> bool {
    let base = match dir.is_empty() {
        true => worktree.to_path_buf(),
        false => worktree.join(dir),
    };
    base.join("BUILD").exists() || base.join("BUILD.bazel").exists()
}

/// True for a file whose deletion `rbuildfiles` can still answer about.
fn is_load_file(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    path.ends_with(".bzl") || base == "BUILD" || base == "BUILD.bazel"
}

/// What the changed paths became, as query roots.
struct Roots {
    roots: Vec<String>,
    /// Deleted paths that could not be named at all.
    unnamed_deletes: usize,
    /// Deleted paths named as their surviving package's `//pkg:*`.
    package_wildcards: usize,
    /// Roots that came back from `rbuildfiles`.
    rbuildfiles_roots: usize,
    /// `rbuildfiles` labels outside any main-repository package. Recorded, not
    /// acted on: the oracle scores main-repository rules only.
    workspace_level: usize,
}

impl WarmQueryStrategy {
    /// Maps the step's changed paths to query roots. The module docs carry the
    /// deleted-path policy.
    fn roots(&self, cx: &StepContext<'_>) -> Roots {
        let paths = cx.changed_set();
        let packages = packages_from_disk(cx.worktree, paths.iter().map(String::as_str));
        let mut out = Roots {
            roots: Vec::new(),
            unnamed_deletes: 0,
            package_wildcards: 0,
            rbuildfiles_roots: 0,
            workspace_level: 0,
        };
        let mut load_files: Vec<String> = Vec::new();

        for path in &paths {
            let exists = cx.worktree.join(path).exists();
            if exists && self.opts.rbuildfiles_first && is_load_file(path) {
                load_files.push(path.clone());
                continue;
            }
            if exists {
                if let Some(pkg) = packages.owner_of(path) {
                    out.roots.push(rdeps::source_label(pkg, path));
                }
                continue;
            }
            // Deleted at head.
            match packages.owner_of(path) {
                Some(pkg) => {
                    out.roots.push(format!("//{pkg}:*"));
                    out.package_wildcards += 1;
                }
                None if is_load_file(path) => load_files.push(path.clone()),
                None => out.unnamed_deletes += 1,
            }
        }

        if !load_files.is_empty() {
            match cx.bazel.rbuildfiles(&load_files) {
                Ok((found, _partial)) => {
                    out.workspace_level = found.workspace_level.len();
                    for pkg in found.packages {
                        out.roots.push(format!("//{pkg}:*"));
                        out.rbuildfiles_roots += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "rbuildfiles failed; those paths go unnamed"
                    );
                    out.unnamed_deletes += load_files.len();
                }
            }
        }

        out.roots.sort();
        out.roots.dedup();
        out
    }
}

impl Strategy for WarmQueryStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    fn prepare(&mut self, cx: &StepContext<'_>) -> Result<Prepared> {
        let start = Instant::now();
        let run = run_query(cx.bazel, "//...", &self.opts)
            .with_context(|| format!("warming the server at {}", cx.parent()))?;
        let wall = start.elapsed();
        self.prepared_pid = server_pid(cx.bazel);
        let mut notes = BTreeMap::new();
        notes.insert("universe_labels".into(), run.labels.len().to_string());
        notes.insert("packages_loaded".into(), run.packages_loaded.to_string());
        notes.insert("partial".into(), run.partial.to_string());
        if let Some(pid) = &self.prepared_pid {
            notes.insert("server_pid".into(), pid.clone());
        }
        Ok(Prepared {
            wall,
            // A warm server holds its state in memory, so there is none to
            // count. That is the honest answer, not a missing one.
            state_bytes: 0,
            notes,
        })
    }

    fn answer(&mut self, cx: &StepContext<'_>) -> Result<Answer> {
        if self.opts.cold {
            // The other half of the number: the same query with nothing warm
            // behind it, which is what a CI job sees.
            cx.bazel.shutdown();
        }
        let roots = self.roots(cx);
        let start = Instant::now();
        let mut targets = BTreeSet::new();
        let mut packages_loaded = 0u64;
        let mut partial = false;
        let mut batches = 0usize;
        for batch in rdeps::batches(&roots.roots, rdeps::MAX_BATCH_LABELS, rdeps::MAX_EXPR_BYTES) {
            batches += 1;
            let expr = rdeps::rdeps_expression(batch);
            let run = run_query(cx.bazel, &expr, &self.opts)?;
            targets.extend(run.labels);
            packages_loaded += run.packages_loaded;
            partial |= run.partial;
        }
        let wall = start.elapsed();
        // The oracle scores main-repository rules only.
        targets.retain(|l| !rdeps::is_external(l));

        let answered_pid = server_pid(cx.bazel);
        let mut notes = BTreeMap::new();
        notes.insert("roots".into(), roots.roots.len().to_string());
        notes.insert("batches".into(), batches.to_string());
        notes.insert("packages_loaded".into(), packages_loaded.to_string());
        notes.insert("partial".into(), partial.to_string());
        notes.insert("unnamed_deletes".into(), roots.unnamed_deletes.to_string());
        notes.insert(
            "package_wildcards".into(),
            roots.package_wildcards.to_string(),
        );
        notes.insert(
            "rbuildfiles_roots".into(),
            roots.rbuildfiles_roots.to_string(),
        );
        notes.insert("workspace_level".into(), roots.workspace_level.to_string());
        notes.insert("cold".into(), self.opts.cold.to_string());
        if let Some(pid) = &answered_pid {
            notes.insert("server_pid".into(), pid.clone());
        }
        notes.insert(
            "same_server".into(),
            (answered_pid.is_some() && answered_pid == self.prepared_pid).to_string(),
        );
        Ok(Answer {
            targets,
            wall,
            notes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_variant_is_read_off_the_strategy_name() {
        assert_eq!(Options::from_name("warm-query"), Some(Options::default()));
        assert_eq!(
            Options::from_name("warm-query-cold"),
            Some(Options {
                cold: true,
                ..Options::default()
            })
        );
        assert_eq!(
            Options::from_name("warm-query-scope-ordered"),
            Some(Options {
                universe_scope: true,
                ordered: true,
                ..Options::default()
            })
        );
        assert_eq!(Options::from_name("warm-query-nonsense"), None);
        assert_eq!(Options::from_name("cambi"), None);
    }

    #[test]
    fn the_default_variant_is_unordered_and_keeps_going() {
        let flags = Options::default().query_flags();
        assert!(flags.iter().any(|f| f == "--order_output=no"));
        assert!(flags.iter().any(|f| f == "--keep_going"));
        assert!(!flags.iter().any(|f| f.starts_with("--universe_scope")));
        // The progress line carries the packages-loaded count, so it stays on.
        assert!(!flags.iter().any(|f| f == "--noshow_progress"));
    }

    #[test]
    fn scoped_and_ordered_variants_say_so() {
        let flags = Options {
            universe_scope: true,
            ordered: true,
            no_keep_going: true,
            ..Options::default()
        }
        .query_flags();
        assert!(flags.iter().any(|f| f == "--universe_scope=//..."));
        assert!(!flags.iter().any(|f| f == "--order_output=no"));
        assert!(!flags.iter().any(|f| f == "--keep_going"));
    }

    #[test]
    fn packages_loaded_is_the_largest_the_progress_lines_reported() {
        let stderr = "Loading: \rLoading: 0 packages loaded\rLoading: 42 packages loaded\r\
                      Loading: 17 packages loaded\nINFO: Empty results\n";
        assert_eq!(packages_loaded(stderr), 42);
        // A server that re-loaded nothing prints no such line.
        assert_eq!(packages_loaded("INFO: Empty results\n"), 0);
        assert_eq!(packages_loaded(""), 0);
    }

    #[test]
    fn packages_come_off_the_worktree_not_an_index() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("src/lib/deep")).unwrap();
        std::fs::write(dir.join("BUILD"), "").unwrap();
        std::fs::write(dir.join("src/lib/BUILD.bazel"), "").unwrap();
        let packages = packages_from_disk(
            dir,
            ["src/lib/deep/a.cc", "src/lib/b.cc", "top.cc", "src/c.cc"],
        );
        assert_eq!(packages.owner_of("src/lib/deep/a.cc"), Some("src/lib"));
        assert_eq!(packages.owner_of("src/lib/b.cc"), Some("src/lib"));
        // The root package owns everything no nearer package claims.
        assert_eq!(packages.owner_of("src/c.cc"), Some(""));
        assert_eq!(packages.owner_of("top.cc"), Some(""));
    }

    #[test]
    fn a_load_file_is_a_bzl_or_a_build_file() {
        assert!(is_load_file("tools/defs.bzl"));
        assert!(is_load_file("src/BUILD"));
        assert!(is_load_file("src/BUILD.bazel"));
        assert!(!is_load_file("src/main.cc"));
        assert!(!is_load_file("src/BUILD.bazel.tpl"));
    }
}
