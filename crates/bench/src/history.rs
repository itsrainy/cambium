//! The updater history (spec §6): cold build at the start of a first-parent
//! range, then `baseline update` for every commit forward to the pin, then a
//! full rebuild at the pin that the applied index must equal.
//!
//! The row source and the checkout are behind [`Driver`], so the history can
//! run over a synthetic git history with no Bazel — which is how it is tested
//! — and over a subject's worktree with a real one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context as _, Result};
use cambium_engine::{AffectedRequest, Engine, IndexQuery, PackageQuerier};
use cambium_index::{format, IndexData, IndexFile};
use serde::{Deserialize, Serialize};

use crate::oracles::rebuild::{compare_by_label, GraphDiff};
use crate::state::{HistoryState, State};
use crate::util::{now_unix, percentile};

/// What the history needs from the world at a revision.
pub trait Driver {
    /// Puts the worktree at `rev`. A no-op when the row source does not read
    /// the worktree.
    fn checkout(&self, rev: &str) -> Result<()>;
    /// The row source for `rev`.
    fn querier(&self, rev: &str) -> Result<Box<dyn PackageQuerier + '_>>;
}

/// A driver over a subject's checkout and a real Bazel.
pub struct BazelDriver<'a> {
    pub checkout: &'a crate::Checkout,
    pub engine: &'a Engine,
}

impl Driver for BazelDriver<'_> {
    fn checkout(&self, rev: &str) -> Result<()> {
        self.checkout.checkout(rev)
    }

    fn querier(&self, rev: &str) -> Result<Box<dyn PackageQuerier + '_>> {
        Ok(Box::new(self.engine.bazel_querier(rev)?))
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
    pub total_ms: u64,
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
    let v = engine.bazel.version();
    v.lines().next().unwrap_or("").trim().to_string()
}

fn tool_sha() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn describe(index: &IndexData, wall_ms: u64) -> Result<ColdBuild> {
    let bytes = format::to_bytes(index)?.len() as u64;
    let v = index.view();
    let stats = IndexQuery::new(v).stats(bytes);
    let c = &index.header.counts;
    Ok(ColdBuild {
        commit: index.provenance.commit.clone(),
        wall_ms,
        index_bytes: bytes,
        nodes: c.nodes,
        edges: c.edges,
        packages: c.packages,
        rules: c.rules,
        tests: c.tests,
        lineage: cambium_engine::inspect::hex(&index.header.lineage),
        partial: cambium_engine::inspect::PartialLoad::from(v),
        stats: serde_json::to_value(stats)?,
    })
}

/// Cold build at `rev`, timed.
pub fn cold_build(
    engine: &Engine,
    driver: &dyn Driver,
    rev: &str,
) -> Result<(IndexData, ColdBuild)> {
    driver.checkout(rev)?;
    let q = driver.querier(rev)?;
    let t = Instant::now();
    let index = engine
        .full_build_with(rev, &*q)
        .with_context(|| format!("cold build at {rev}"))?;
    let wall = t.elapsed().as_millis() as u64;
    let desc = describe(&index, wall)?;
    Ok((index, desc))
}

fn publish(engine: &Engine, index: &IndexData) -> Result<()> {
    if engine.store.is_none() {
        return Ok(());
    }
    engine
        .publish_base_index(index)
        .with_context(|| format!("publishing the index for {}", index.provenance.commit))?;
    Ok(())
}

/// Loads the index a previous run published for `rev`.
fn load_published(engine: &Engine, rev: &str) -> Result<Option<IndexData>> {
    let Some(path) = engine.fetch_baseline(rev)? else {
        return Ok(None);
    };
    Ok(Some(IndexFile::open(&path)?.deserialize()?))
}

/// The history proper. `commits` is the first-parent range, oldest first, with
/// the pin last; the cold build happens at `commits[0]`.
pub fn history(
    engine: &Engine,
    driver: &dyn Driver,
    commits: &[String],
    opts: &HistoryOptions,
) -> Result<HistoryResult> {
    anyhow::ensure!(commits.len() >= 2, "a history needs at least two commits");
    let from = commits[0].clone();
    let to = commits[commits.len() - 1].clone();
    let path = HistoryResult::path_versioned(
        &opts.results_dir,
        &opts.pin,
        Some(opts.bazel_version.as_str()),
    );
    let mut state = State::load_for(
        &opts.state_dir,
        &opts.repo_id,
        &opts.pin,
        &State::engine_id(&engine.cfg.hash()),
    )?;

    // ---- resume, or start ----
    let (mut result, mut index, start_at) = match (opts.resume, HistoryResult::read(&path)) {
        (true, Ok(prev))
            if prev.from == from
                && prev.to == to
                && !prev.done
                && prev.engine == State::engine_id(&engine.cfg.hash()) =>
        {
            let last = prev
                .commits
                .last()
                .map(|c| c.commit.clone())
                .unwrap_or_else(|| from.clone());
            match load_published(engine, &last)? {
                Some(idx) => {
                    let pos = commits.iter().position(|c| *c == last).unwrap_or(0);
                    tracing::info!(%last, done = prev.commits.len(), "resuming history");
                    (prev, idx, pos + 1)
                }
                None => {
                    tracing::warn!(%last, "no published index to resume from; starting over");
                    start(engine, driver, &from, &to, opts)?
                }
            }
        }
        (true, Ok(prev))
            if prev.done
                && prev.from == from
                && prev.to == to
                && prev.engine == State::engine_id(&engine.cfg.hash()) =>
        {
            tracing::info!("history already complete");
            return Ok(prev);
        }
        _ => start(engine, driver, &from, &to, opts)?,
    };
    state.history = Some(HistoryState {
        from: from.clone(),
        to: to.clone(),
        bazel_version: opts.bazel_version.clone(),
        last_completed: result.commits.last().map(|c| c.commit.clone()),
        done: false,
    });
    state.save(&opts.state_dir)?;

    // ---- walk forward ----
    for i in start_at..commits.len() {
        let (parent, commit) = (&commits[i - 1], &commits[i]);
        driver.checkout(commit)?;
        let changes = engine.repo.diff(parent, commit)?;
        let q = driver.querier(commit)?;
        let req = AffectedRequest {
            base: parent.clone(),
            head: commit.clone(),
            include_manual: true,
            explain: false,
            no_full_build: true,
            materialize: true,
            publish_base: false,
            max_depth: None,
        };
        let t = Instant::now();
        let out = engine
            .compute(index.view(), &changes, &*q, &req)
            .with_context(|| format!("update {} -> {}", &parent[..12.min(parent.len())], commit))?;
        let mut total_ms = t.elapsed().as_millis() as u64;
        let r = &out.report;
        let mut rec = CommitRecord {
            commit: commit.clone(),
            parent: parent.clone(),
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
        index = match out.index {
            Some(idx) => idx,
            None => {
                // Everything was invalidated: the only sound continuation is a
                // fresh lineage at this commit.
                tracing::info!(%commit, reason = ?r.global_reason, "global invalidation; cold rebuild");
                let (idx, cold) = cold_build(engine, driver, commit)?;
                total_ms += cold.wall_ms;
                rec.total_ms = total_ms;
                rec.cold_rebuild = true;
                idx
            }
        };
        rec.index_nodes = index.header.counts.nodes;
        rec.index_edges = index.header.counts.edges;
        if opts.publish {
            publish(engine, &index)?;
        }
        result.commits.push(rec);
        result.summarise();
        result.write(&path)?;
        if let Some(rs) = state.history.as_mut() {
            rs.last_completed = Some(commit.clone());
        }
        state.save(&opts.state_dir)?;
        tracing::info!(
            step = i,
            of = commits.len() - 1,
            %commit,
            dirty = result.commits.last().map(|c| c.dirty_packages).unwrap_or(0),
            ms = total_ms,
            "applied"
        );
    }

    // ---- the rebuild oracle: applied == rebuilt ----
    if opts.rebuild_oracle {
        let (fresh, rebuild) = cold_build(engine, driver, &to)?;
        let diff = compare_by_label(index.view(), fresh.view(), opts.diff_cap);
        let ok = diff.is_empty();
        result.rebuild = Some(rebuild);
        result.oracle = Some(OracleOutcome { ok, diff });
    } else {
        tracing::warn!("the rebuild oracle was skipped: this history proves nothing");
    }
    result.done = true;
    result.summarise();
    result.write(&path)?;
    if let Some(rs) = state.history.as_mut() {
        rs.done = true;
    }
    state.save(&opts.state_dir)?;
    Ok(result)
}

fn start(
    engine: &Engine,
    driver: &dyn Driver,
    from: &str,
    to: &str,
    opts: &HistoryOptions,
) -> Result<(HistoryResult, IndexData, usize)> {
    let (index, cold) = cold_build(engine, driver, from)?;
    if opts.publish {
        publish(engine, &index)?;
    }
    let result = HistoryResult {
        schema: crate::RESULT_SCHEMA,
        repo: opts.repo_id.clone(),
        pin: opts.pin.clone(),
        from: from.to_string(),
        to: to.to_string(),
        bazel_version: opts.bazel_version.clone(),
        bazel_version_used: bazel_version_used(engine),
        tool_sha: tool_sha(),
        engine: State::engine_id(&engine.cfg.hash()),
        started_unix: now_unix(),
        cold,
        commits: Vec::new(),
        rebuild: None,
        oracle: None,
        summary: Summary::default(),
        done: false,
    };
    result.write(&HistoryResult::path_versioned(
        &opts.results_dir,
        &opts.pin,
        Some(opts.bazel_version.as_str()),
    ))?;
    Ok((result, index, 1))
}

/// Per-commit records keyed by commit, for the report and the comparison.
pub fn records_by_commit(r: &HistoryResult) -> BTreeMap<&str, &CommitRecord> {
    r.commits.iter().map(|c| (c.commit.as_str(), c)).collect()
}
