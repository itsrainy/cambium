//! The updater history (spec §6): cold build at the start of a first-parent
//! range, then `baseline update` for every commit forward to the pin, then a
//! full rebuild at the pin that the applied index must equal.
//!
//! Both halves are behind [`Driver`], so the walk itself is one piece of code
//! over two ways of driving cambium: [`CambiDriver`] runs the `cambi` binary
//! the way a user would (`cambi index build`, `cambi affected --format json`,
//! `cambi index publish`), which is what a real subject's run uses and what
//! the published numbers therefore measure; [`EngineDriver`] drives the engine
//! in process over a synthetic row source, which is how the walk is tested
//! with no Bazel and no product binary anywhere.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use cambium_engine::report::AffectedReport;
use cambium_engine::{AffectedRequest, Engine, IndexQuery, PackageQuerier};
use cambium_index::{IndexFile, IndexRef};
use serde::{Deserialize, Serialize};

use crate::oracles::rebuild::{compare_by_label, GraphDiff};
use crate::state::{HistoryState, State};
use crate::util::{now_unix, percentile};

/// What the history needs from the world at a revision.
///
/// Every method is an operation a user performs: put the worktree here, build
/// an index from scratch, bring one forward. The walk never holds an index in
/// memory — each step writes one to a file and the next step's base comes from
/// the store, exactly as it would for someone running `cambi` twice.
pub trait Driver {
    /// Puts the worktree at `rev`. A no-op when the row source does not read
    /// the worktree.
    fn checkout(&self, rev: &str) -> Result<()>;

    /// Cold build at `rev`, written to `out` and published when asked. The
    /// duration is what the person running it waited for.
    fn build(&self, rev: &str, out: &Path, publish: bool) -> Result<Duration>;

    /// Brings the index for `base` forward to `head`, writing the result to
    /// `out` and publishing it when asked.
    fn update(&self, base: &str, head: &str, out: &Path, publish: bool) -> Result<Step>;
}

/// One forward step: the answer, what it cost, and whether an index came out
/// of it.
pub struct Step {
    pub report: AffectedReport,
    /// Wall time for the step itself, not counting the publish that follows
    /// it: the publish is an upload, and a history quoting it as the cost of
    /// an update would be quoting the network.
    pub wall: Duration,
    /// False when the range invalidated everything: there is no head index,
    /// and the caller cold-rebuilds.
    pub wrote_index: bool,
}

/// cambium as a user runs it: the `cambi` binary, over a subject's checkout.
pub struct CambiDriver<'a> {
    pub checkout: &'a crate::checkout::Checkout,
    pub cambi: &'a crate::tools::cambi::Cambi,
}

impl Driver for CambiDriver<'_> {
    fn checkout(&self, rev: &str) -> Result<()> {
        self.checkout.checkout(rev)
    }

    fn build(&self, rev: &str, out: &Path, publish: bool) -> Result<Duration> {
        self.cambi
            .index_build(&self.checkout.dir, rev, out, publish)
    }

    fn update(&self, base: &str, head: &str, out: &Path, publish: bool) -> Result<Step> {
        // `cambi affected`, not `cambi index update`: the two do the same work
        // — `update` is `affected` with its head index kept — and only
        // `affected` prints the document the per-commit record is made of.
        let run = self.cambi.affected(
            &self.checkout.dir,
            &crate::tools::cambi::AffectedArgs {
                include_manual: true,
                out: Some(out.to_path_buf()),
                ..crate::tools::cambi::AffectedArgs::new(base, head)
            },
        )?;
        let wrote_index = run.index.is_some();
        if publish && wrote_index {
            self.cambi.index_publish(&self.checkout.dir, out)?;
        }
        Ok(Step {
            report: run.report,
            wall: run.wall,
            wrote_index,
        })
    }
}

/// Where a driver that does not run Bazel gets its package rows.
pub trait RowSource {
    /// Puts the worktree at `rev`, if it has one.
    fn checkout(&self, _rev: &str) -> Result<()> {
        Ok(())
    }
    /// The row source for `rev`.
    fn querier(&self, rev: &str) -> Result<Box<dyn PackageQuerier + '_>>;
}

/// cambium in process, over a row source: no product binary, no Bazel.
///
/// This is the test harness's driver, and the one place in the benchmark where
/// the engine answers the product's question directly. It exists so the walk —
/// resume, the oracle, the per-commit record — can be tested over a synthetic
/// git history on a machine with no Bazel; it is never what a published number
/// was measured with.
pub struct EngineDriver<'a> {
    pub engine: &'a Engine,
    pub rows: &'a dyn RowSource,
}

impl EngineDriver<'_> {
    fn publish_file(&self, path: &Path) -> Result<()> {
        if self.engine.store().is_none() {
            return Ok(());
        }
        let index = IndexFile::open(path)?.deserialize()?;
        self.engine
            .publish_base_index(&index)
            .with_context(|| format!("publishing the index for {}", index.provenance.commit))?;
        Ok(())
    }
}

impl Driver for EngineDriver<'_> {
    fn checkout(&self, rev: &str) -> Result<()> {
        self.rows.checkout(rev)
    }

    fn build(&self, rev: &str, out: &Path, publish: bool) -> Result<Duration> {
        let q = self.rows.querier(rev)?;
        let t = Instant::now();
        let index = self
            .engine
            .full_build_with(rev, &*q)
            .with_context(|| format!("cold build at {rev}"))?;
        let wall = t.elapsed();
        cambium_index::format::write_index(out, &index)?;
        if publish {
            self.publish_file(out)?;
        }
        Ok(wall)
    }

    fn update(&self, base: &str, head: &str, out: &Path, publish: bool) -> Result<Step> {
        let base_path = self
            .engine
            .fetch_baseline(base)?
            .with_context(|| format!("no published index for {base}"))?;
        let base_index = IndexFile::open(&base_path)?;
        let changes = self.engine.repo().diff(base, head)?;
        let q = self.rows.querier(head)?;
        let req = AffectedRequest {
            base: base.to_string(),
            head: head.to_string(),
            include_manual: true,
            explain: false,
            no_full_build: true,
            materialize: true,
            publish_base: false,
            allow_partial: false,
            max_depth: None,
        };
        let t = Instant::now();
        let out_run = self
            .engine
            .compute(base_index.view(), &changes, &*q, &req)
            .with_context(|| format!("update {} -> {head}", &base[..12.min(base.len())]))?;
        let wall = t.elapsed();
        let wrote_index = match &out_run.index {
            Some(i) => {
                cambium_index::format::write_index(out, i)?;
                true
            }
            None => false,
        };
        if publish && wrote_index {
            self.publish_file(out)?;
        }
        Ok(Step {
            report: out_run.report,
            wall,
            wrote_index,
        })
    }
}

#[derive(Clone, Debug)]
pub struct HistoryOptions {
    pub repo_id: String,
    pub pin: String,
    /// Where `history-<pin>.json` goes: `bench-results/<subject>/history/`.
    pub results_dir: PathBuf,
    /// Where `state.json` goes: `bench-results/<subject>/`. One state per
    /// subject, shared by every workload, because what it records — the
    /// fetch, the cold builds, the engine identity — is a property of the
    /// subject and not of the workload asking.
    pub state_dir: PathBuf,
    /// Publish every index to the engine's store (needed for resume, and for
    /// the comparison change sets to find their base without a cold build).
    pub publish: bool,
    /// Continue a history whose state file says it stopped part way.
    pub resume: bool,
    pub bazel_version: String,
    /// How many differences the oracle report lists per class.
    pub diff_cap: usize,
    /// Run the rebuild oracle at the end: cold-build the pin and diff it
    /// against the index the history produced. `--oracle none` turns it off,
    /// which is only ever right when the cold build is the thing that will not
    /// finish — without it the history proves nothing.
    pub rebuild_oracle: bool,
    /// `cambi --version` of the binary being measured, recorded in the result.
    pub cambi_version: Option<String>,
    /// Where to report progress, for `cambium-bench watch`. `None` runs the
    /// walk exactly as before and writes no progress file.
    pub progress: Option<std::sync::Arc<crate::progress::Reporter>>,
}

/// One cold build.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ColdBuild {
    pub commit: String,
    pub wall_ms: u64,
    pub index_bytes: u64,
    pub nodes: u32,
    pub edges: u64,
    pub packages: u32,
    pub rules: u32,
    pub tests: u32,
    pub lineage: String,
    /// Whether Bazel loaded the whole repository for this build. A partial
    /// cold build is not a failure — the incremental path drops exactly the
    /// same packages — but every number above describes a repository minus
    /// those packages, so it travels with them.
    #[serde(default)]
    pub partial: cambium_engine::inspect::PartialLoad,
    /// `cambi index stats` for the index, verbatim.
    pub stats: serde_json::Value,
}

/// One `baseline update` step.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitRecord {
    pub commit: String,
    pub parent: String,
    pub changed_files: usize,
    pub ignored_files: usize,
    pub dirty_packages: usize,
    pub queried_targets: usize,
    pub dirty_ms: u64,
    pub query_ms: u64,
    pub splice_ms: u64,
    pub direct_ms: u64,
    pub walk_ms: u64,
    /// Whole-process wall time for the step: what a user waits for.
    pub total_ms: u64,
    /// The engine's own `timings_ms.total` from the document, beside the wall
    /// time above. 0 in results written before the harness ran `cambi` as a
    /// child process.
    #[serde(default)]
    pub engine_total_ms: u64,
    pub direct: usize,
    pub affected: usize,
    pub tests: usize,
    /// Kept for continuity with earlier history runs; always false since the
    /// static load closure replaced the `rbuildfiles` call.
    pub rbuildfiles_called: bool,
    /// Packages dirtied because a changed `.bzl` is in their load closure.
    #[serde(default)]
    pub bzl_closure_hits: usize,
    pub global: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_reason: Option<String>,
    /// The commit invalidated everything and a cold build minted a new
    /// lineage; `total_ms` is then the cold build.
    pub cold_rebuild: bool,
    /// The engine itself re-indexed `//...` at head, because the dirty set
    /// exceeded `limits.max_dirty_packages`. Unlike `cold_rebuild` above, the
    /// commit's answer is exact and the lineage survives — which is why the
    /// two are counted apart. Absent from results written before the limit
    /// existed, where it reads as `false`.
    #[serde(default)]
    pub engine_cold_rebuild: bool,
    pub index_nodes: u32,
    pub index_edges: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Summary {
    pub commits: usize,
    pub p50_total_ms: u64,
    pub p90_total_ms: u64,
    pub p99_total_ms: u64,
    pub max_total_ms: u64,
    pub p50_query_ms: u64,
    pub p90_query_ms: u64,
    /// Fraction of commits whose diff dirtied no package (source-only edits).
    pub zero_dirty_fraction: f64,
    pub largest_dirty_packages: usize,
    pub largest_dirty_commit: String,
    pub globals: usize,
    /// Commits the engine answered with a whole-repository re-query. Not
    /// globals: the answer was exact.
    #[serde(default)]
    pub engine_cold_rebuilds: usize,
    pub rbuildfiles_calls: usize,
    /// Commits where a changed `.bzl` dirtied at least one package through the
    /// static load closure.
    #[serde(default)]
    pub bzl_closure_commits: usize,
    pub mean_affected: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OracleOutcome {
    pub ok: bool,
    pub diff: GraphDiff,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryResult {
    pub schema: u32,
    pub repo: String,
    pub pin: String,
    pub from: String,
    pub to: String,
    /// The `--bazel-version` override the run was asked for, if any. It is a
    /// *request* (`7.x`), not what bazelisk resolved it to.
    pub bazel_version: String,
    /// What `bazel --version` actually reported in the worktree, e.g.
    /// `bazel 7.4.1`. Empty when the binary could not be asked (and absent
    /// entirely from results written before this field existed), which the
    /// report renders as `n/a`.
    #[serde(default)]
    pub bazel_version_used: String,
    pub tool_sha: String,
    /// `cambi --version` of the binary this walk measured, verbatim. The
    /// harness runs `cambi` as a child process, so a result names the binary
    /// that produced it. Absent from results written before it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cambi_version: Option<String>,
    /// `State::engine_id` of the build that wrote this; a resume by any other
    /// build starts over.
    #[serde(default)]
    pub engine: String,
    pub started_unix: u64,
    pub cold: ColdBuild,
    pub commits: Vec<CommitRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebuild: Option<ColdBuild>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle: Option<OracleOutcome>,
    /// Where the index the rebuild oracle cold-built at the pin was archived,
    /// `<results>/<subject>/heads/<pin sha>.bin`. That index is Bazel's truth
    /// at the pin, and keeping it is what lets `cambium-bench replay
    /// --head-check` re-derive the externals tables from the head tree's text
    /// with a later engine and compare them against it, offline. `None` when
    /// the oracle did not run, when the archive already existed, or in every
    /// result written before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_index: Option<PathBuf>,
    pub summary: Summary,
    pub done: bool,
}

impl HistoryResult {
    pub fn path(results_dir: &Path, pin: &str) -> PathBuf {
        results_dir.join(format!("history-{pin}.json"))
    }

    /// The result file for one Bazel version of the matrix; `None` (or an
    /// empty version) is the repo's own `.bazelversion` and keeps the plain
    /// name.
    pub fn path_versioned(results_dir: &Path, pin: &str, version: Option<&str>) -> PathBuf {
        results_dir.join(crate::util::versioned_name(
            &format!("history-{pin}"),
            version,
            "json",
        ))
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

    pub fn summarise(&mut self) {
        self.summary = summarise(&self.commits);
    }
}

pub fn summarise(commits: &[CommitRecord]) -> Summary {
    let mut totals: Vec<u64> = commits.iter().map(|c| c.total_ms).collect();
    totals.sort_unstable();
    let mut queries: Vec<u64> = commits.iter().map(|c| c.query_ms).collect();
    queries.sort_unstable();
    let largest = commits.iter().max_by_key(|c| c.dirty_packages);
    Summary {
        commits: commits.len(),
        p50_total_ms: percentile(&totals, 0.5),
        p90_total_ms: percentile(&totals, 0.9),
        p99_total_ms: percentile(&totals, 0.99),
        max_total_ms: totals.last().copied().unwrap_or(0),
        p50_query_ms: percentile(&queries, 0.5),
        p90_query_ms: percentile(&queries, 0.9),
        zero_dirty_fraction: if commits.is_empty() {
            0.0
        } else {
            commits
                .iter()
                .filter(|c| c.dirty_packages == 0 && !c.global)
                .count() as f64
                / commits.len() as f64
        },
        largest_dirty_packages: largest.map(|c| c.dirty_packages).unwrap_or(0),
        largest_dirty_commit: largest.map(|c| c.commit.clone()).unwrap_or_default(),
        globals: commits.iter().filter(|c| c.global).count(),
        engine_cold_rebuilds: commits.iter().filter(|c| c.engine_cold_rebuild).count(),
        rbuildfiles_calls: commits.iter().filter(|c| c.rbuildfiles_called).count(),
        bzl_closure_commits: commits.iter().filter(|c| c.bzl_closure_hits > 0).count(),
        mean_affected: if commits.is_empty() {
            0.0
        } else {
            commits.iter().map(|c| c.affected as f64).sum::<f64>() / commits.len() as f64
        },
    }
}

/// `bazel --version` in the subject's worktree, so the report can say which Bazel
/// actually answered rather than which one was asked for. Best effort: an
/// unavailable binary yields an empty string rather than failing the history.
fn bazel_version_used(engine: &Engine) -> String {
    let v = engine.bazel().version();
    v.lines().next().unwrap_or("").trim().to_string()
}

fn describe(v: IndexRef<'_>, bytes: u64, wall_ms: u64) -> Result<ColdBuild> {
    let stats = IndexQuery::new(v).stats(bytes);
    Ok(ColdBuild {
        commit: v.commit.to_string(),
        wall_ms,
        index_bytes: bytes,
        nodes: v.counts().nodes,
        edges: v.counts().edges,
        packages: v.counts().packages,
        rules: v.counts().rules,
        tests: v.counts().tests,
        lineage: cambium_engine::inspect::hex(&v.lineage),
        partial: cambium_engine::inspect::PartialLoad::from(v),
        stats: serde_json::to_value(stats)?,
    })
}

/// Cold build at `rev`, timed, into `out`.
pub fn cold_build(driver: &dyn Driver, rev: &str, out: &Path, publish: bool) -> Result<ColdBuild> {
    driver.checkout(rev)?;
    let wall = driver.build(rev, out, publish)?;
    let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    let f = IndexFile::open(out)?;
    describe(f.view(), bytes, wall.as_millis() as u64)
}

/// The history proper. `commits` is the first-parent range, oldest first, with
/// the pin last; the cold build happens at `commits[0]`.
///
/// Three phases: resume the recorded history or cold-build the start of the
/// range; walk forward one commit at a time, writing the result after each;
/// then the rebuild oracle at the pin.
pub fn history(
    engine: &Engine,
    driver: &dyn Driver,
    commits: &[String],
    opts: &HistoryOptions,
) -> Result<HistoryResult> {
    anyhow::ensure!(commits.len() >= 2, "a history needs at least two commits");
    let from = commits[0].clone();
    let to = commits[commits.len() - 1].clone();
    let engine_id = State::engine_id(&engine.config().hash());
    let state = State::load_for(&opts.state_dir, &opts.repo_id, &opts.pin, &engine_id)?;
    // Where each step's index lands. One file, rewritten every commit: the
    // walk's own continuity is the store, which is where the next step's base
    // comes from, and this is only what the record's counts and the oracle's
    // comparison are read from.
    std::fs::create_dir_all(&opts.state_dir)?;
    let index_path = opts.state_dir.join(crate::util::versioned_name(
        &format!("walk-{}", opts.pin),
        Some(opts.bazel_version.as_str()),
        "bin",
    ));

    let (result, start_at) = match resume(engine, commits, opts, &engine_id)? {
        Resume::Done(prev) => return Ok(prev),
        Resume::From(prev, at) => (prev, at),
        Resume::Fresh => start(engine, driver, &from, &to, &index_path, opts)?,
    };
    let mut walk = Walk {
        engine,
        driver,
        opts,
        path: result_path(opts),
        index_path,
        result,
        state,
    };
    walk.begin(commits.len(), &from, &to)?;
    for i in start_at..commits.len() {
        walk.step(i, commits.len() - 1, &commits[i - 1], &commits[i])?;
    }
    if opts.rebuild_oracle {
        walk.rebuild_oracle(commits.len() - 1, &to)?;
    } else {
        tracing::warn!("the rebuild oracle was skipped: this history proves nothing");
    }
    walk.finish()
}

/// `history-<pin>[-<version>].json`.
fn result_path(opts: &HistoryOptions) -> PathBuf {
    HistoryResult::path_versioned(
        &opts.results_dir,
        &opts.pin,
        Some(opts.bazel_version.as_str()),
    )
}

/// What a previous result file allows.
enum Resume {
    /// The same range, the same cambium, finished: nothing to do.
    Done(HistoryResult),
    /// The same range and cambium, part way: continue at this position.
    From(HistoryResult, usize),
    /// Not resuming, or nothing recorded to resume.
    Fresh,
}

/// Whether the recorded history can be picked up: same range, same cambium,
/// and the store still holds the index the last completed commit left.
fn resume(
    engine: &Engine,
    commits: &[String],
    opts: &HistoryOptions,
    engine_id: &str,
) -> Result<Resume> {
    if !opts.resume {
        return Ok(Resume::Fresh);
    }
    // Nothing recorded is nothing to continue: a resumed run over a subject it
    // has not reached yet starts that subject. Anything recorded that cannot
    // be continued is an error, never a silent fresh start.
    let path = result_path(opts);
    if !path.exists() {
        return Ok(Resume::Fresh);
    }
    let prev = HistoryResult::read(&path)
        .map_err(|e| anyhow::anyhow!("cannot resume {}: {e:#}; pass --restart", path.display()))?;
    let from = &commits[0];
    let to = &commits[commits.len() - 1];
    anyhow::ensure!(
        prev.from == *from && prev.to == *to,
        "cannot resume {}: it walked {}..{} and this walk is {from}..{to}; pass --restart",
        path.display(),
        prev.from,
        prev.to
    );
    anyhow::ensure!(
        prev.engine == engine_id,
        "cannot resume {}: {} recorded it and this build is {engine_id}; pass --restart",
        path.display(),
        prev.engine
    );
    if prev.done {
        tracing::info!("history already complete");
        return Ok(Resume::Done(prev));
    }
    let last = prev
        .commits
        .last()
        .map(|c| c.commit.clone())
        .unwrap_or_else(|| from.clone());
    // The index to carry forward is whichever one the store holds for
    // `last`; the next `update` asks the store for it by name, so there is
    // nothing to load here — only to check that it is there.
    match engine.fetch_baseline(&last)? {
        Some(_) => {
            let pos = commits.iter().position(|c| *c == last).unwrap_or(0);
            tracing::info!(%last, done = prev.commits.len(), "resuming history");
            Ok(Resume::From(prev, pos + 1))
        }
        None => anyhow::bail!(
            "cannot resume {}: the store no longer holds the index for {last}; pass --restart",
            path.display()
        ),
    }
}

/// A history in progress: the result and state files it rewrites after every
/// commit, and the index file each step lands in.
struct Walk<'a> {
    engine: &'a Engine,
    driver: &'a dyn Driver,
    opts: &'a HistoryOptions,
    path: PathBuf,
    index_path: PathBuf,
    result: HistoryResult,
    state: State,
}

impl Walk<'_> {
    fn progress(&self) -> Option<&crate::progress::Reporter> {
        self.opts.progress.as_deref()
    }

    /// Records that the walk is under way: the progress file and the state
    /// file's history entry.
    fn begin(&mut self, commits: usize, from: &str, to: &str) -> Result<()> {
        if let Some(p) = self.progress() {
            p.step(crate::progress::Step::History);
            p.total(commits.saturating_sub(1));
            if let Some(last) = self.result.commits.last() {
                p.item(self.result.commits.len(), &last.commit);
            }
        }
        self.state.stamp_start("history");
        self.state.history = Some(HistoryState {
            from: from.to_string(),
            to: to.to_string(),
            bazel_version: self.opts.bazel_version.clone(),
            last_completed: self.result.commits.last().map(|c| c.commit.clone()),
            done: false,
        });
        self.state.save(&self.opts.state_dir)
    }

    /// One commit forward: the update, a cold rebuild where it invalidated
    /// everything, and the record written before the next.
    fn step(&mut self, i: usize, of: usize, parent: &str, commit: &str) -> Result<()> {
        if let Some(p) = self.progress() {
            p.item(i, commit);
            p.tool(Some(crate::tools::cambi::NAME));
        }
        self.driver.checkout(commit)?;
        let step = match self
            .driver
            .update(parent, commit, &self.index_path, self.opts.publish)
        {
            Ok(s) => s,
            Err(e) => {
                // The walk stops either way; the progress file is what says
                // so to whoever is watching, since the error itself goes to
                // this process's stderr and nowhere a reader can see it.
                if let Some(p) = self.progress() {
                    p.failed(&format!("{e:#}"));
                }
                return Err(e);
            }
        };
        // Whole-process wall time: what someone bringing their index forward
        // one commit waits for. The engine's own total travels beside it,
        // from the document, so the process overhead is visible rather than
        // charged silently to the engine.
        let mut total_ms = step.wall.as_millis() as u64;
        let r = &step.report;
        let mut rec = CommitRecord {
            commit: commit.to_string(),
            parent: parent.to_string(),
            changed_files: r.stats.changed_files,
            ignored_files: r.stats.ignored_files,
            dirty_packages: r.stats.dirty_packages,
            queried_targets: r.stats.queried_targets,
            dirty_ms: r.timings_ms.dirty,
            query_ms: r.timings_ms.query,
            splice_ms: r.timings_ms.splice,
            direct_ms: r.timings_ms.direct,
            walk_ms: r.timings_ms.walk,
            total_ms,
            engine_total_ms: r.timings_ms.total,
            direct: r.stats.direct_targets,
            affected: r.stats.affected_targets,
            tests: r.tests.len(),
            rbuildfiles_called: r.stats.rbuildfiles_called,
            bzl_closure_hits: r.stats.bzl_closure_hits,
            global: r.global,
            global_reason: r.global_reason.clone(),
            cold_rebuild: false,
            engine_cold_rebuild: r.stats.cold_rebuild,
            index_nodes: 0,
            index_edges: 0,
        };
        if !step.wrote_index {
            // Everything was invalidated: the only sound continuation is a
            // fresh lineage at this commit.
            tracing::info!(%commit, reason = ?r.global_reason, "global invalidation; cold rebuild");
            let cold = cold_build(self.driver, commit, &self.index_path, self.opts.publish)?;
            total_ms += cold.wall_ms;
            rec.total_ms = total_ms;
            rec.cold_rebuild = true;
        }
        {
            let f = IndexFile::open(&self.index_path)?;
            rec.index_nodes = f.view().counts().nodes;
            rec.index_edges = f.view().counts().edges;
        }
        if let Some(p) = self.progress() {
            p.diagnostics(&step.report.diagnostics);
            p.tool(None);
        }
        self.result.commits.push(rec);
        self.result.summarise();
        self.result.write(&self.path)?;
        if let Some(rs) = self.state.history.as_mut() {
            rs.last_completed = Some(commit.to_string());
        }
        self.state.stamp("history", "updated");
        self.state.save(&self.opts.state_dir)?;
        tracing::info!(
            step = i,
            of,
            %commit,
            dirty = self.result.commits.last().map(|c| c.dirty_packages).unwrap_or(0),
            ms = total_ms,
            "applied"
        );
        Ok(())
    }

    /// The rebuild oracle: applied == rebuilt. A fresh cold build at the pin
    /// is compared against the index the walk produced, and kept.
    fn rebuild_oracle(&mut self, at: usize, to: &str) -> Result<()> {
        if let Some(p) = self.progress() {
            p.item(at, "rebuild oracle");
        }
        let fresh_path = self.index_path.with_extension("rebuild.bin");
        // The oracle's own cold build is never published: it would overwrite
        // the index the walk just proved, with an index the walk did not
        // produce.
        let rebuild = cold_build(self.driver, to, &fresh_path, false)?;
        let applied = IndexFile::open(&self.index_path)?;
        let fresh = IndexFile::open(&fresh_path)?;
        let diff = compare_by_label(applied.view(), fresh.view(), self.opts.diff_cap);
        let ok = diff.is_empty();
        // Keep the index Bazel just built. Comparing it against the applied
        // one answers today's question and then throws away the only artefact
        // that says what Bazel's graph and externals tables *are* at the pin;
        // archived, a later engine's derivation can be checked against it on a
        // laptop (`cambium-bench replay --head-check`). `state_dir` is
        // `bench-results/<subject>/`, beside the change-set driver's `bases/`.
        self.result.head_index =
            archive_head_index(self.engine, &self.opts.state_dir, to, &fresh_path);
        self.result.rebuild = Some(rebuild);
        self.result.oracle = Some(OracleOutcome { ok, diff });
        Ok(())
    }

    /// Marks the history done in both files and hands the result back.
    fn finish(mut self) -> Result<HistoryResult> {
        self.result.done = true;
        self.result.summarise();
        self.result.write(&self.path)?;
        if let Some(rs) = self.state.history.as_mut() {
            rs.done = true;
        }
        self.state.stamp("history", "done");
        self.state.save(&self.opts.state_dir)?;
        if let Some(p) = self.progress() {
            p.done();
        }
        Ok(self.result)
    }
}

/// Archives the oracle's freshly built head index, best effort.
///
/// A history that answered correctly must not fail because a diagnostic copy
/// could not be written, so a failure is a warning and a `None`.
fn archive_head_index(
    engine: &Engine,
    state_dir: &Path,
    pin: &str,
    fresh: &Path,
) -> Option<PathBuf> {
    let dest = crate::replay::archived_head_path(state_dir, pin);
    match crate::replay::archive_index(
        &dest,
        crate::replay::ArchiveSource::File(fresh),
        engine.index_compression(),
    ) {
        Ok(Some(at)) => {
            let bytes = std::fs::metadata(&at).map(|m| m.len()).unwrap_or(0);
            tracing::info!(%pin, path = %at.display(), bytes, "archived the head index the rebuild oracle built");
            Some(at)
        }
        Ok(None) => {
            tracing::info!(%pin, path = %dest.display(), "the head index was already archived");
            None
        }
        Err(e) => {
            tracing::warn!(%pin, error = format!("{e:#}"), "could not archive the head index");
            None
        }
    }
}

fn start(
    engine: &Engine,
    driver: &dyn Driver,
    from: &str,
    to: &str,
    index_path: &Path,
    opts: &HistoryOptions,
) -> Result<(HistoryResult, usize)> {
    let cold = cold_build(driver, from, index_path, opts.publish)?;
    let result = HistoryResult {
        schema: crate::RESULT_SCHEMA,
        repo: opts.repo_id.clone(),
        pin: opts.pin.clone(),
        from: from.to_string(),
        to: to.to_string(),
        bazel_version: opts.bazel_version.clone(),
        bazel_version_used: bazel_version_used(engine),
        tool_sha: crate::util::tool_sha().to_string(),
        cambi_version: opts.cambi_version.clone(),
        engine: State::engine_id(&engine.config().hash()),
        started_unix: now_unix(),
        cold,
        commits: Vec::new(),
        rebuild: None,
        oracle: None,
        head_index: None,
        summary: Summary::default(),
        done: false,
    };
    result.write(&HistoryResult::path_versioned(
        &opts.results_dir,
        &opts.pin,
        Some(opts.bazel_version.as_str()),
    ))?;
    Ok((result, 1))
}

/// Per-commit records keyed by commit, for the report and the comparison.
pub fn records_by_commit(r: &HistoryResult) -> BTreeMap<&str, &CommitRecord> {
    r.commits.iter().map(|c| (c.commit.as_str(), c)).collect()
}
