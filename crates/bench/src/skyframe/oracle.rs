//! The `skyframe-oracle` strategy: Bazel's own graph, on one warm server, as
//! the ground truth for what a change touched.
//!
//! The shape is the one the prototype notes describe. `prepare` loads the
//! subject at the parent on the harness's server and dumps the graph;
//! `answer` re-loads at head on the *same* server and dumps again; the answer
//! is read out of the difference between the two dumps.
//!
//! Two loads are implemented, because they turn out to answer different
//! questions:
//!
//! - [`Load::Loading`], the default, is `bazel query 'kind(rule, //...)'`. It
//!   is loading-phase only, so the graph holds `PACKAGE`, `BZL_LOAD`, `GLOBS`,
//!   `DIRECTORY_LISTING` and `FILE_STATE` nodes and no targets at all. The
//!   query's own output doubles as the rule universe at head, which the answer
//!   needs to turn a dirty package into labels.
//! - [`Load::Analysis`] is `bazel build --nobuild --keep_going //...`, which
//!   adds `CONFIGURED_TARGET` nodes. Those carry the real target graph: a
//!   source file is a configured target of its own, and the edges between
//!   configured targets are what `bazel query rdeps` walks. `--keep_going` is
//!   not optional on the `bazel` subject, whose `//...` holds targets that
//!   fail analysis.
//!
//! Pick one with `CAMBIUM_SKYFRAME_LOAD=loading|analysis`.
//!
//! Two answers are computed from every pair of dumps and both are recorded,
//! so the notes say which is tighter at equal recall:
//!
//! - *package-diff*: the rules of every main-repository package whose
//!   `PACKAGE` node is new, gone, or has a different dependency set.
//! - *rdeps*: the transitive reverse closure of the head dump from the
//!   `FILE_STATE` keys of the step's changed paths, plus every node the diff
//!   named, mapped back to labels.
//!
//! The answer the harness scores is the union, which is the tighter one when
//! they agree and the sound one when they do not.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use anyhow::{Context as _, Result};

use super::dump::{self, Dump};
use super::{Answer, Prepared, StepContext, Strategy};

/// How the server is warmed before the graph is dumped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Load {
    /// `bazel query 'kind(rule, //...)'`: loading phase only.
    Loading,
    /// `bazel build --nobuild --keep_going //...`: loading and analysis.
    Analysis,
}

impl Load {
    pub fn name(self) -> &'static str {
        match self {
            Load::Loading => "loading",
            Load::Analysis => "analysis",
        }
    }

    /// `CAMBIUM_SKYFRAME_LOAD`, defaulting to the loading-only variant.
    pub fn from_env() -> Self {
        match std::env::var("CAMBIUM_SKYFRAME_LOAD").as_deref() {
            Ok("analysis") => Load::Analysis,
            _ => Load::Loading,
        }
    }
}

/// The keys the dump is filtered to.
///
/// The filter is a regex over the whole key string, so this alternation also
/// admits `PACKAGE_LOOKUP`, `PACKAGE_ERROR_MESSAGE` and
/// `DIRECTORY_LISTING_STATE`. That is wanted: they are edges in the same
/// graph, and dropping them would break paths the reverse walk needs.
pub const SKYKEY_FILTER: &str =
    "^(PACKAGE|CONFIGURED_TARGET|TARGET|BZL_LOAD|GLOBS|FILE_STATE|DIRECTORY_LISTING)";

pub struct SkyframeOracle {
    pub load: Load,
}

impl Default for SkyframeOracle {
    fn default() -> Self {
        Self {
            load: Load::from_env(),
        }
    }
}

/// What one `prepare` or `answer` side's Bazel work produced.
struct Side {
    dump_path: PathBuf,
    rules_path: PathBuf,
    load_ms: u64,
    dump_ms: u64,
    dump_bytes: u64,
    server_pid: String,
}

impl SkyframeOracle {
    /// Warms the server, dumps the graph, and records what each half cost.
    fn side(&self, cx: &StepContext<'_>, which: &str) -> Result<Side> {
        let dump_path = cx.scratch.join(format!("{which}.dump"));
        let rules_path = cx.scratch.join(format!("{which}.rules"));

        let t = Instant::now();
        match self.load {
            Load::Loading => {
                // The query is both the warm-up and the rule universe.
                run_to_file(
                    cx,
                    &["query", "kind(rule, //...)", "--keep_going"],
                    &rules_path,
                )?;
            }
            Load::Analysis => {
                run_to_file(
                    cx,
                    &["build", "--nobuild", "--keep_going", "//..."],
                    &cx.scratch.join(format!("{which}.build.log")),
                )?;
                // The label set still comes from a query; on a server this
                // warm it costs a second or two.
                run_to_file(
                    cx,
                    &["query", "kind(rule, //...)", "--keep_going"],
                    &rules_path,
                )?;
            }
        }
        let load_ms = t.elapsed().as_millis() as u64;

        // Asked between the load and the dump, so a note that the server
        // restarted names the command that restarted it.
        let server_pid = server_pid(cx);

        let t = Instant::now();
        run_to_file(
            cx,
            &[
                "dump",
                "--skyframe=deps",
                &format!("--skykey_filter={SKYKEY_FILTER}"),
            ],
            &dump_path,
        )?;
        let dump_ms = t.elapsed().as_millis() as u64;
        let dump_bytes = std::fs::metadata(&dump_path).map(|m| m.len()).unwrap_or(0);
        anyhow::ensure!(
            dump_bytes > 0,
            "the {which} dump is empty; the server lost its graph between the load and the dump"
        );

        Ok(Side {
            dump_path,
            rules_path,
            load_ms,
            dump_ms,
            dump_bytes,
            server_pid,
        })
    }
}

impl Strategy for SkyframeOracle {
    fn name(&self) -> &str {
        "skyframe-oracle"
    }

    fn prepare(&mut self, cx: &StepContext<'_>) -> Result<Prepared> {
        let t = Instant::now();
        let side = self.side(cx, "parent")?;
        let wall = t.elapsed();

        let parsed = Dump::read(&side.dump_path)?;
        let mut notes = BTreeMap::new();
        notes.insert("load".into(), self.load.name().into());
        notes.insert("load_ms".into(), side.load_ms.to_string());
        notes.insert("dump_ms".into(), side.dump_ms.to_string());
        notes.insert("dump_bytes".into(), side.dump_bytes.to_string());
        notes.insert("nodes".into(), parsed.len().to_string());
        notes.insert("server_pid".into(), side.server_pid.clone());
        for (kind, n) in parsed.counts() {
            notes.insert(format!("nodes.{kind}"), n.to_string());
        }
        // So `answer` can say whether the server it dumped is this one.
        record_pid(cx.scratch, &side.server_pid)?;

        Ok(Prepared {
            wall,
            // The dump on disk is the only thing carried between the calls;
            // the graph itself lives in the server and costs nothing here.
            state_bytes: side.dump_bytes,
            notes,
        })
    }

    fn answer(&mut self, cx: &StepContext<'_>) -> Result<Answer> {
        let t = Instant::now();
        let side = self.side(cx, "head")?;

        let parent = Dump::read(&cx.scratch.join("parent.dump"))?;
        let head = Dump::read(&side.dump_path)?;
        let rules = read_rules(&side.rules_path)?;

        let changed = dump::diff(&parent, &head);
        let answers = Answers::compute(cx.worktree, &parent, &head, &changed, cx, &rules);
        let wall = t.elapsed();

        let mut notes = BTreeMap::new();
        notes.insert("load".into(), self.load.name().into());
        notes.insert("load_ms".into(), side.load_ms.to_string());
        notes.insert("dump_ms".into(), side.dump_ms.to_string());
        notes.insert("dump_bytes".into(), side.dump_bytes.to_string());
        notes.insert("nodes".into(), head.len().to_string());
        notes.insert("server_pid".into(), side.server_pid.clone());
        notes.insert(
            "server_warm".into(),
            (side.server_pid == parent_pid(cx).unwrap_or_default() && !side.server_pid.is_empty())
                .to_string(),
        );
        notes.insert("rule_universe".into(), rules.len().to_string());
        for (kind, n) in head.counts() {
            notes.insert(format!("nodes.{kind}"), n.to_string());
        }
        notes.insert("diff_nodes".into(), changed.len().to_string());
        for (kind, n) in count_by_kind(&changed) {
            notes.insert(format!("diff.{kind}"), n.to_string());
        }
        notes.insert(
            "dirty_packages".into(),
            answers.dirty_packages.len().to_string(),
        );
        notes.insert(
            "package_diff_targets".into(),
            answers.package_diff.len().to_string(),
        );
        notes.insert("rdeps_targets".into(), answers.rdeps.len().to_string());
        notes.insert("seeds".into(), answers.seeds.to_string());
        notes.insert("tighter".into(), answers.tighter().into());

        Ok(Answer {
            targets: answers.union(),
            wall,
            notes,
        })
    }
}

/// The two answers the dump pair supports, kept apart so the notes can say
/// which one carried the result.
struct Answers {
    dirty_packages: BTreeSet<String>,
    /// The rules of every package whose `PACKAGE` node is not identical.
    package_diff: BTreeSet<String>,
    /// The reverse closure of the head dump from the changed paths.
    rdeps: BTreeSet<String>,
    /// How many seeds the closure actually started from.
    seeds: usize,
}

impl Answers {
    fn compute(
        worktree: &Path,
        _parent: &Dump,
        head: &Dump,
        changed: &BTreeMap<String, dump::Change>,
        cx: &StepContext<'_>,
        rules: &BTreeSet<String>,
    ) -> Self {
        // Every package the diff names, whichever side of the diff it came
        // from. A removed `PACKAGE` node is as much a dirty package as a
        // rewired one.
        let dirty_packages: BTreeSet<String> = changed
            .keys()
            .filter_map(|k| dump::main_repo_package(k))
            .map(str::to_string)
            .collect();

        // The seeds: the `FILE_STATE` key of every changed path, plus the key
        // of every directory above it, because a path that is new or gone
        // changes its parent's listing and not its own state.
        let mut seeds: BTreeSet<String> = BTreeSet::new();
        for c in cx.changed {
            seeds.insert(dump::file_state_key(worktree, &c.path));
            let mut rest = c.path.as_str();
            while let Some((dir, _)) = rest.rsplit_once('/') {
                seeds.insert(dump::file_state_key(worktree, dir));
                rest = dir;
            }
            seeds.insert(dump::file_state_key(worktree, ""));
        }
        // Whatever the diff already proved dirty is a seed too, so a node the
        // path walk cannot name (a `BZL_LOAD` under another repository, say)
        // still propagates.
        seeds.extend(changed.keys().cloned());
        let seed_count = seeds.len();

        let reached = head.reverse_closure(&seeds);

        // Packages the closure reached, and configured targets it reached.
        let mut reached_packages: BTreeSet<&str> = BTreeSet::new();
        let mut reached_labels: BTreeSet<&str> = BTreeSet::new();
        for key in &reached {
            if let Some(pkg) = dump::main_repo_package(key) {
                reached_packages.insert(pkg);
            }
            if let Some(label) = dump::configured_target_label(key) {
                if label.starts_with("//") {
                    reached_labels.insert(label);
                }
            }
        }

        let package_diff = rules_in(rules, &dirty_packages.iter().map(|s| s.as_str()).collect());
        let mut rdeps = rules_in(rules, &reached_packages);
        // On an analysis-tier dump the configured targets are the precise
        // answer; on a loading-only one there are none and this adds nothing.
        rdeps.extend(
            reached_labels
                .into_iter()
                .filter(|l| rules.contains(*l))
                .map(str::to_string),
        );

        Self {
            dirty_packages,
            package_diff,
            rdeps,
            seeds: seed_count,
        }
    }

    fn union(&self) -> BTreeSet<String> {
        self.package_diff.union(&self.rdeps).cloned().collect()
    }

    /// Which of the two is the smaller set, for the record.
    fn tighter(&self) -> &'static str {
        match self.package_diff.len().cmp(&self.rdeps.len()) {
            std::cmp::Ordering::Less => "package-diff",
            std::cmp::Ordering::Greater => "rdeps",
            std::cmp::Ordering::Equal => "equal",
        }
    }
}

/// Every rule label whose package is in `packages`.
fn rules_in(rules: &BTreeSet<String>, packages: &BTreeSet<&str>) -> BTreeSet<String> {
    rules
        .iter()
        .filter(|label| {
            dump::label_package(label)
                .map(|p| packages.contains(p))
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

fn count_by_kind(changed: &BTreeMap<String, dump::Change>) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for key in changed.keys() {
        *counts.entry(dump::kind(key).to_string()).or_insert(0) += 1;
    }
    counts
}

/// `kind(rule, //...)`'s output as a label set.
fn read_rules(path: &Path) -> Result<BTreeSet<String>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the rule universe at {}", path.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("//"))
        .map(str::to_string)
        .collect())
}

/// Runs a Bazel command through the harness's wrapper, stdout to `out`.
///
/// The exit status is deliberately not checked. `query --keep_going` and
/// `build --keep_going` both exit non-zero on a subject whose `//...` holds
/// something that does not load or analyse, and on the `bazel` subject they
/// always do; what matters is whether the output is there, which every caller
/// checks for itself.
fn run_to_file(cx: &StepContext<'_>, args: &[&str], out: &Path) -> Result<()> {
    let file = std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut cmd = cx.bazel.command(args[0]);
    cmd.args(&args[1..]);
    cmd.stdout(Stdio::from(file));
    cmd.stderr(Stdio::null());
    cmd.stdin(Stdio::null());
    cmd.status()
        .with_context(|| format!("spawning bazel {}", args.join(" ")))?;
    Ok(())
}

/// `bazel info server_pid`, empty when it cannot be had.
fn server_pid(cx: &StepContext<'_>) -> String {
    let mut cmd = cx.bazel.command("info");
    cmd.arg("server_pid").stderr(Stdio::null());
    match cmd.output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => String::new(),
    }
}

/// The pid recorded by `prepare`, read back out of the scratch directory.
fn parent_pid(cx: &StepContext<'_>) -> Option<String> {
    std::fs::read_to_string(cx.scratch.join("parent.pid"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Written by `prepare` so `answer` can tell whether the server survived.
pub fn record_pid(scratch: &Path, pid: &str) -> Result<()> {
    std::fs::write(scratch.join("parent.pid"), pid)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_dirty_package_pulls_in_its_rules_and_no_others() {
        let universe = rules(&["//a:x", "//a:y", "//b:z", "//a/c:w"]);
        let picked = rules_in(&universe, &["a"].into_iter().collect());
        // `//a/c` is a package of its own, not part of `//a`.
        assert_eq!(picked, rules(&["//a:x", "//a:y"]));
    }

    #[test]
    fn the_root_package_is_a_package_like_any_other() {
        let universe = rules(&["//:root", "//a:x"]);
        assert_eq!(
            rules_in(&universe, &[""].into_iter().collect()),
            rules(&["//:root"])
        );
    }

    #[test]
    fn the_diff_is_counted_by_node_kind() {
        let mut changed = BTreeMap::new();
        changed.insert("PACKAGE:a".to_string(), dump::Change::Added);
        changed.insert("PACKAGE:b".to_string(), dump::Change::Removed);
        changed.insert(
            "FILE_STATE:[/w]/[a/BUILD]".to_string(),
            dump::Change::DepsChanged,
        );
        let counts = count_by_kind(&changed);
        assert_eq!(counts["PACKAGE"], 2);
        assert_eq!(counts["FILE_STATE"], 1);
    }

    #[test]
    fn the_rule_universe_ignores_everything_that_is_not_a_label() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rules");
        std::fs::write(
            &p,
            "//a:x\nLoading: 3 packages loaded\n//b:y\n\n@@ext//c:z\n",
        )
        .unwrap();
        assert_eq!(read_rules(&p).unwrap(), rules(&["//a:x", "//b:y"]));
    }

    #[test]
    fn the_load_variant_comes_from_the_environment_and_defaults_to_loading() {
        assert_eq!(Load::Loading.name(), "loading");
        assert_eq!(Load::Analysis.name(), "analysis");
        // Anything the variable is not becomes the loading-only variant.
        assert!(!Load::from_env().name().is_empty());
    }
}
