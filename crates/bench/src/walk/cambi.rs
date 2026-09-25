//! Driving cambium for a walk: the `cambi` binary over a real subject, or the
//! engine in process over a synthetic row source for the test.
//!
//! Two ways to drive one walk. [`CambiDriver`] runs the `cambi` binary the way
//! a user would (`cambi index build`, `cambi affected --format json`, `cambi
//! index publish`), which is what a real subject's run measures. [`EngineDriver`]
//! drives the engine in process over a [`RowSource`], with no Bazel and no
//! product binary, which is how the walk loop is tested over a synthetic git
//! history.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use cambium_engine::report::AffectedReport;
use cambium_engine::{AffectedRequest, Engine, IndexQuery, PackageQuerier};
use cambium_index::{IndexFile, IndexRef};
use serde::{Deserialize, Serialize};

/// What a walk needs from the world at a revision.
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
    fn update(&self, base: &str, head: &str, out: &Path, publish: bool) -> Result<Update>;

    /// Every path in the tree at `rev`, for the rebuild oracle to classify
    /// its loading-layer rows against ([`super::rebuild_rows`]); `None` for
    /// a driver with no tree, which leaves every missing row unexplained.
    fn paths(&self, _rev: &str) -> Result<Option<BTreeSet<String>>> {
        Ok(None)
    }

    /// Stops the Bazel server this driver's builds run on, so the next
    /// [`build`](Self::build) starts on an empty Skyframe graph. The output
    /// base stays, and with it every repository already fetched into it: a
    /// restarted server finds the downloaded ones' marker files up to date
    /// and does not fetch them again (local and configure repositories run
    /// again, as they do on any server start). An error when the server could not be stopped, since
    /// the caller is about to rely on the graph being empty.
    ///
    /// The rebuild oracle's reference build is what needs it (see
    /// `Runner::reference_build` in the walk driver). A no-op for a driver
    /// with no server, whose every build is fresh already.
    fn restart_server(&self) -> Result<()> {
        Ok(())
    }
}

/// `git ls-tree -r --name-only` at `rev`, NUL-separated so a path with a
/// space or a quote reads back as itself.
pub fn tree_paths(dir: &Path, rev: &str) -> Result<BTreeSet<String>> {
    let out = crate::checkout::git(dir, &["ls-tree", "-r", "--name-only", "-z", rev])?;
    Ok(out
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect())
}

/// One forward step's result: the answer, its affected set, what it cost, and
/// whether an index came out of it.
pub struct Update {
    pub report: AffectedReport,
    /// The affected set, whole: what the record keeps only the count of.
    pub labels: std::collections::BTreeSet<String>,
    /// Wall time for the step, not counting the publish that follows it: the
    /// publish is an upload, and quoting it as the cost of an update would be
    /// quoting the network.
    pub wall: Duration,
    /// False when the range invalidated everything: there is no head index,
    /// and the caller cold-rebuilds.
    pub wrote_index: bool,
}

/// cambium as a user runs it: the `cambi` binary, over a subject's checkout.
pub struct CambiDriver<'a> {
    pub checkout: &'a crate::checkout::Checkout,
    pub cambi: &'a crate::tools::cambi::Cambi,
    /// The Bazel `cambi` runs in the checkout: the same workspace, startup
    /// flags and environment, so the same server. Only
    /// [`Driver::restart_server`] uses it; `cambi` starts its own.
    pub bazel: &'a cambium_bazel::Bazel,
}

impl Driver for CambiDriver<'_> {
    fn checkout(&self, rev: &str) -> Result<()> {
        self.checkout.checkout(rev)
    }

    fn build(&self, rev: &str, out: &Path, publish: bool) -> Result<Duration> {
        self.cambi
            .index_build(&self.checkout.dir, rev, out, publish)
    }

    fn update(&self, base: &str, head: &str, out: &Path, publish: bool) -> Result<Update> {
        // The same request the tier's cost model measures.
        let run = self.cambi.affected(
            &self.checkout.dir,
            &crate::tools::cambi::AffectedArgs::harness(base, head, out),
        )?;
        let wrote_index = run.index.is_some();
        if publish && wrote_index {
            self.cambi.index_publish(&self.checkout.dir, out)?;
        }
        Ok(Update {
            labels: run.report.affected_labels().into_iter().collect(),
            report: run.report,
            wall: run.wall,
            wrote_index,
        })
    }

    fn paths(&self, rev: &str) -> Result<Option<BTreeSet<String>>> {
        tree_paths(&self.checkout.dir, rev).map(Some)
    }

    fn restart_server(&self) -> Result<()> {
        stop_server(self.bazel, SERVER_STOP_WAIT)
    }
}

/// How long a stopped server gets to exit after `bazel shutdown` returns.
const SERVER_STOP_WAIT: Duration = Duration::from_secs(60);

/// `bazel shutdown` on `bazel`'s output base, checked: the command has to
/// succeed and, when the startup flags name the output base, no server may
/// be left running on it by `wait`.
///
/// Not [`cambium_bazel::Bazel::shutdown`], which is best effort by design
/// (it runs on the way out of a run, where a failure spoils nothing). Here
/// a server that survives would serve the next build its whole history.
pub fn stop_server(bazel: &cambium_bazel::Bazel, wait: Duration) -> Result<()> {
    let mut cmd = bazel.command("shutdown");
    let out = cmd
        .output()
        .with_context(|| format!("running bazel shutdown in {}", bazel.workspace().display()))?;
    anyhow::ensure!(
        out.status.success(),
        "bazel shutdown in {} exited with {}: {}",
        bazel.workspace().display(),
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let Some(base) = bazel
        .startup_flags()
        .iter()
        .find_map(|f| f.strip_prefix("--output_base="))
    else {
        return Ok(());
    };
    let base = Path::new(base);
    let deadline = Instant::now() + wait;
    while let Some(pid) = crate::servers::running_on(base) {
        anyhow::ensure!(
            Instant::now() < deadline,
            "bazel shutdown returned but the server on {} (pid {pid}) is still running after {}s",
            base.display(),
            wait.as_secs()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    tracing::info!(output_base = %base.display(), "stopped the bazel server for a fresh graph");
    Ok(())
}

/// Where a driver that does not run Bazel gets its package rows.
pub trait RowSource {
    /// Puts the worktree at `rev`, if it has one.
    fn checkout(&self, _rev: &str) -> Result<()> {
        Ok(())
    }
    /// The row source for `rev`.
    fn querier(&self, rev: &str) -> Result<Box<dyn PackageQuerier + '_>>;
    /// The tree at `rev`, if it has one; see [`Driver::paths`].
    fn paths(&self, _rev: &str) -> Result<Option<BTreeSet<String>>> {
        Ok(None)
    }
}

/// cambium in process, over a row source: no product binary, no Bazel.
///
/// The test harness's driver, and the one place in the benchmark where the
/// engine answers the product's question directly. It exists so the walk loop
/// can be tested over a synthetic git history on a machine with no Bazel; it
/// is never what a published number was measured with.
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

    fn update(&self, base: &str, head: &str, out: &Path, publish: bool) -> Result<Update> {
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
            include_manual: crate::tools::cambi::INCLUDE_MANUAL,
            explain: true,
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
        Ok(Update {
            labels: out_run.report.affected_labels().into_iter().collect(),
            report: out_run.report,
            wall,
            wrote_index,
        })
    }

    fn paths(&self, rev: &str) -> Result<Option<BTreeSet<String>>> {
        self.rows.paths(rev)
    }
}

/// One cold build's shape, for the walk's index-build record and the rebuild
/// oracle.
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
    #[serde(default)]
    pub partial: cambium_engine::inspect::PartialLoad,
    /// `cambi index stats` for the index, verbatim.
    pub stats: serde_json::Value,
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

/// Archives the rebuild oracle's freshly built head index, best effort.
///
/// A walk that answered correctly must not fail because a diagnostic copy
/// could not be written, so a failure is a warning and a `None`.
pub fn archive_head_index(
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

#[cfg(test)]
mod tests {
    use super::*;
    use cambium_testkit::{commit_all, init_repo, write};

    /// The tree at each revision, whole, with a path a line-split would
    /// have mangled.
    #[test]
    fn the_tree_at_a_revision_lists_every_path_including_one_with_a_space() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        init_repo(&root);
        write(&root, "BUILD", "");
        write(&root, "lib/a.cc", "");
        let first = commit_all(&root, "one");
        write(&root, "new dir/with space.txt", "");
        write(&root, "lib/b.cc", "");
        std::fs::remove_file(root.join("lib/a.cc")).unwrap();
        let second = commit_all(&root, "two");

        let at_first = tree_paths(&root, &first).unwrap();
        assert_eq!(
            at_first.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["BUILD", "lib/a.cc"]
        );
        let at_second = tree_paths(&root, &second).unwrap();
        assert_eq!(
            at_second.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["BUILD", "lib/b.cc", "new dir/with space.txt"]
        );
        assert!(tree_paths(&root, "nosuchrev").is_err());
    }
}
