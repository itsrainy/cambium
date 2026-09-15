//! What each verb does once its arguments are parsed.
//!
//! The clap files marshal arguments; this is where a `subjects fetch`, a
//! `history` or a `changes` actually happens, against a [`SubjectRun`] the
//! session resolved. Nothing here prints: every function answers with the
//! paths and documents it produced, and the binary decides how to show them
//! (`--format`, exit codes). That is what lets the launcher, the `run` verb
//! and a test drive the same steps without a terminal.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::labels::{self, ClassRow, SetDiff};
use crate::progress::Step;
use crate::report;
use crate::session::{Session, SubjectRun};
use crate::tools::{self, ToolLocations};

// ---------------------------------------------------------------- subjects

/// `subjects fetch`: the mirror and the worktree at the pin. Answers with the
/// worktree.
pub fn fetch(run: &SubjectRun<'_>) -> Result<PathBuf> {
    let mut state = run.state()?;
    let progress = run.progress("subjects", Step::Fetch, None);
    tracing::info!(id = %run.entry.id, url = %run.entry.url, pin = %run.entry.pin, "fetching");
    progress.item(1, &run.entry.pin);
    let checkout = run
        .session
        .subjects
        .fetch(&run.entry)
        .map_err(|e| {
            progress.failed(&format!("{e:#}"));
            e
        })
        .with_context(|| format!("fetching subject {}", run.entry.id))?;
    let cfg = checkout.write_config(&run.entry, run.version())?;
    state.fetched = true;
    state.stamp("fetch", "done");
    state.save(&run.results)?;
    progress.done();
    tracing::info!(worktree = %checkout.dir.display(), config = %cfg.display(), "fetched");
    Ok(checkout.dir)
}

/// `subjects build`: the cold index at `at` (default: the pin), published to
/// the subject's store. Answers with the index file. A warning the index
/// carries (a partial load) is returned beside it for the binary to show.
pub fn build(run: &SubjectRun<'_>, at: Option<&str>, force: bool) -> Result<Built> {
    let checkout = run.checkout()?;
    let progress = run.progress("subjects", Step::Build, None);
    let want = at.unwrap_or(&run.entry.pin);
    let sha = checkout
        .repo
        .rev_parse(want)
        .unwrap_or_else(|_| want.into());
    let out = run.index_path(&sha);
    let mut state = run.state()?;
    if !force {
        // "Built" means this build can read it: an index left by an older
        // format is a rebuild, not an answer.
        if let Some(prev) = state.built.get(&sha) {
            match cambium_index::IndexFile::open(prev) {
                Ok(_) => {
                    tracing::info!(%sha, path = %prev.display(), "already built");
                    progress.done();
                    return Ok(Built {
                        index: prev.clone(),
                        warning: None,
                    });
                }
                Err(e) => tracing::warn!(path = %prev.display(), error = %e, "rebuilding"),
            }
        }
    }
    checkout.checkout(&sha)?;
    // The configuration the subject's runs share has to be on disk before a
    // child `cambi` can read it.
    checkout.write_config(&run.entry, run.version())?;
    let cambi = run.cambi(&checkout)?;
    tracing::info!(%sha, cambi = %cambi.version(), "cold build: cambi index build (this takes a while)");
    progress.item(1, &sha);
    progress.tool(Some(crate::tools::cambi::NAME));
    let wall = cambi
        .index_build(&checkout.dir, &sha, &out, true)
        .map_err(|e| {
            progress.failed(&format!("{e:#}"));
            e
        })?;
    let index = cambium_index::IndexFile::open(&out)?;
    let v = index.view();
    let c = v.counts();
    tracing::info!(
        %sha,
        nodes = c.nodes,
        edges = c.edges,
        packages = c.packages,
        rules = c.rules,
        tests = c.tests,
        secs = wall.as_secs(),
        path = %out.display(),
        "built"
    );
    let warning = cambium_engine::inspect::PartialLoad::from(v).warning();
    if let Some(w) = &warning {
        tracing::warn!("{w}");
    }
    state.built.insert(sha, out.clone());
    state.stamp("build", "done");
    state.save(&run.results)?;
    progress.done();
    Ok(Built {
        index: out,
        warning,
    })
}

/// What `subjects build` answers with.
#[derive(Clone, Debug)]
pub struct Built {
    pub index: PathBuf,
    /// The index loaded partially, and a run over it under-reports.
    pub warning: Option<String>,
}

/// What a workload does with a result already recorded for its pin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnExisting {
    /// Refuse to start. A recorded result is never continued or thrown away
    /// without being asked: continuing one silently is how a crashed run's
    /// numbers once passed for a fresh run's.
    #[default]
    Refuse,
    /// `--resume`: continue it, and fail if it cannot be continued (another
    /// build's, another range's) rather than start over.
    Resume,
    /// `--restart`: discard it and start again.
    Restart,
}

impl OnExisting {
    pub fn from_flags(resume: bool, restart: bool) -> Self {
        match (resume, restart) {
            (true, _) => OnExisting::Resume,
            (_, true) => OnExisting::Restart,
            _ => OnExisting::Refuse,
        }
    }
}

/// Refuses when `path` holds a recorded result and nobody said what to do
/// with it. `recorded` counts what is in it, for the message.
fn refuse_existing(
    path: &Path,
    what: &str,
    on: OnExisting,
    recorded: impl FnOnce() -> Option<usize>,
) -> Result<()> {
    if on != OnExisting::Refuse || !path.exists() {
        return Ok(());
    }
    let n = recorded()
        .map(|n| format!(" ({n} recorded)"))
        .unwrap_or_default();
    anyhow::bail!(
        "{} already holds a {what}{n}; pass --resume to continue it or --restart to discard it",
        path.display()
    )
}

// -------------------------------------------------------------------- walk

/// What `walk` was asked to do.
#[derive(Clone, Debug, Default)]
pub struct WalkRequest {
    pub tier: String,
    /// `--steps`; the subject's window when absent.
    pub steps: Option<usize>,
    /// `--peers`, as typed; empty is the tier's default, `none` turns them
    /// off.
    pub peers: Vec<String>,
    pub overrides: crate::walk::Overrides,
    pub on_existing: OnExisting,
}

/// What a walk answered with.
#[derive(Clone, Debug)]
pub struct Walked {
    pub result: crate::walk::WalkResult,
    pub path: PathBuf,
}

/// The one walk: cambium and, per the tier's schedule, the rdeps oracle and
/// the peers, over a first-parent window to the pin.
pub fn run_walk(
    run: &SubjectRun<'_>,
    req: &WalkRequest,
    locations: &ToolLocations,
) -> Result<Walked> {
    let tier = crate::walk::Tier::parse(&req.tier)
        .ok_or_else(|| anyhow::anyhow!("unknown tier {:?}: correctness, full", req.tier))?;
    let schedule = tier.schedule(&req.overrides);

    // The peers are the named tools, cambium excluded (it is the driver, not
    // a peer). `--peers none` and a schedule with peers off both mean none.
    let peers_off = req.peers.iter().any(|p| p == "none") || !schedule.peers;
    let wanted = if peers_off {
        Vec::new()
    } else {
        tools::wanted(&req.peers)?
    };
    let owned: Vec<Box<dyn tools::Tool>> = wanted
        .iter()
        .map(|n| tools::open(n, locations))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    let tool_refs: Vec<&dyn tools::Tool> = owned.iter().map(|p| p.as_ref()).collect();

    let checkout = run.checkout()?;
    let engine = checkout.engine(&run.entry, run.version())?;
    let _servers = crate::servers::StopOnDrop::subject(
        &checkout.dir,
        &run.results.join("scratch"),
        engine.bazel().startup_flags(),
    );
    let cambi = run.cambi(&checkout)?;

    let range = req
        .steps
        .or(run.session.quick.then_some(crate::QUICK_COMMITS))
        .unwrap_or(run.entry.history_window);
    let (paths, globs) = run.entry.invalidator_rules();
    let probes = crate::walk::Probes {
        // Best effort: three of the four content tags need no index, and a
        // fresh subject has none. Missing hubs cost the `hub` tag its steps
        // and nothing else.
        hub_packages: run.hub_packages().unwrap_or_default(),
        invalidator_paths: paths,
        invalidator_globs: globs,
    };
    let (commits, steps) = crate::walk::steps(&checkout.repo, &run.entry.pin, range, &probes)?;
    anyhow::ensure!(steps.len() >= 1, "a walk needs at least one step");

    let oracle_sample = if schedule.oracle_every_step {
        crate::walk::Sample::all(&steps)
    } else {
        crate::walk::sample(
            &steps,
            run.session.seed,
            schedule.per_tag,
            Some(schedule.oracle_sample),
        )
    };
    let peer_sample = if peers_off {
        crate::walk::Sample::default()
    } else {
        match schedule.peer_sample {
            Some(n) => crate::walk::sample(&steps, run.session.seed, schedule.per_tag, Some(n)),
            None => crate::walk::Sample::all(&steps),
        }
    };

    let results = run.workload("walk")?;
    let bazel_used = engine.bazel().version();
    let environment = crate::env::Environment {
        machine: crate::harness::Machine::detect(),
        bazel_version_used: bazel_used.lines().next().unwrap_or("").trim().to_string(),
        cambi_version: cambi.version().to_string(),
        tool_sha: crate::util::tool_sha().to_string(),
        engine_id: String::new(),
        config_hash: cambium_engine::inspect::hex(&engine.config().hash()),
        output_bases: Default::default(),
    };
    let progress = run.progress("walk", Step::Walk, Some(cambi.version().to_string()));
    let opts = crate::walk::WalkOptions {
        repo_id: run.entry.id.clone(),
        pin: run.entry.pin.clone(),
        results_dir: results.clone(),
        state_dir: run.results.clone(),
        bazel_version: run.version().unwrap_or_default().to_string(),
        schedule,
        oracle_sample,
        peer_sample,
        environment,
        publish: true,
        resume: req.on_existing == OnExisting::Resume,
        diff_cap: 50,
        progress: Some(progress),
    };
    let path = crate::walk::WalkResult::path(&results, &run.entry.pin, run.version());
    refuse_existing(&path, "walk", req.on_existing, || {
        crate::walk::WalkResult::read(&path)
            .ok()
            .map(|w| w.steps.len())
    })?;

    let driver = crate::walk::CambiDriver {
        checkout: &checkout,
        cambi: &cambi,
    };
    let live = crate::walk::BazelChecks {
        engine: &engine,
        checkout: &checkout,
        tools: &tool_refs,
        results_dir: &results,
        scratch: &run.results.join("scratch"),
        oracle_k: 0,
        seed: run.session.seed,
    };
    tracing::info!(
        tier = tier.name(),
        steps = steps.len(),
        peers = tool_refs.len(),
        oracle_steps = opts.oracle_sample.len(),
        "walking"
    );
    let result = crate::walk::walk(&engine, &driver, &live, &commits, &steps, &opts)?;
    Ok(Walked { result, path })
}

/// A one-line summary of a walk, for the terminal.
pub fn walk_summary(r: &crate::walk::WalkResult) -> String {
    let s = &r.summary;
    format!(
        "{} steps, {} with the oracle, worst recall {:.4}, walk p50 {} ms; {}",
        s.steps,
        s.steps_with_oracle,
        s.soundness.worst_recall,
        s.cost.cambium_walk.p50_ms,
        if s.pass { "PASS" } else { "FAIL" }
    )
}

/// The change sets to compare: an explicit file, else the one a previous run
/// wrote for this pin, else a fresh selection with the requested kinds.
// ------------------------------------------------------------------ labels

/// What `labels` asks about one change set's stored sets.
#[derive(Clone, Debug, Default)]
pub struct LabelsQuery {
    /// `base..head`; abbreviated revisions are fine.
    pub change_set: String,
    /// One stored set, or the tool a disagreement was with.
    pub tool: Option<String>,
    /// Only disagreements filed in this class.
    pub class: Option<String>,
    /// Only disagreements on this side: `tool` or `cambium`.
    pub side: Option<String>,
    /// How each tool's set stands against cambium's, instead of the labels.
    pub diff: bool,
}

/// What `labels` answers with.
#[derive(Clone, Debug)]
pub enum Labels {
    /// `--diff`: per tool, only-tool / only-cambium / both.
    Diff(Vec<SetDiff>),
    /// `--class` or `--side`: the classified disagreements that match.
    Classes(Vec<ClassRow>),
    /// The stored sets themselves, by name.
    Sets(Vec<(String, BTreeSet<String>)>),
}

/// `labels`: the stored sets of one change set, filtered. Offline: nothing
/// here runs a Bazel or opens a checkout.
pub fn read_labels(run: &SubjectRun<'_>, q: &LabelsQuery) -> Result<Labels> {
    let dir = labels::resolve(&run.workload_path("walk"), &q.change_set)?;
    if q.diff {
        return Ok(Labels::Diff(labels::diff(&dir)?));
    }
    let filter = labels::Filter {
        tool: q.tool.clone(),
        class: q
            .class
            .as_deref()
            .map(|c| {
                labels::parse_class(c)
                    .ok_or_else(|| anyhow::anyhow!("no disagreement class named {c}"))
            })
            .transpose()?,
        side: q
            .side
            .as_deref()
            .map(|s| {
                labels::parse_side(s).ok_or_else(|| anyhow::anyhow!("--side is tool or cambium"))
            })
            .transpose()?,
    };
    // A class or a side is a question about the classification; a bare --tool,
    // or nothing at all, is a question about what a tool reported.
    if filter.class.is_some() || filter.side.is_some() {
        let path = labels::classes_path(&dir).with_context(|| {
            format!(
                "{} holds no classification; a globally invalidated change set has none",
                dir.display()
            )
        })?;
        return Ok(Labels::Classes(filter.apply(&labels::read_classes(&path)?)));
    }
    let sets = labels::stored_sets(&dir)?;
    let wanted: Vec<_> = match &q.tool {
        Some(name) => {
            let hit = sets.iter().find(|(n, _)| n == name).with_context(|| {
                format!(
                    "{} has no set named {name}; it has {}",
                    dir.display(),
                    sets.iter()
                        .map(|(n, _)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            vec![hit.clone()]
        }
        None => sets,
    };
    wanted
        .into_iter()
        .map(|(name, path)| Ok((name, labels::read_set(&path)?)))
        .collect::<Result<Vec<_>>>()
        .map(Labels::Sets)
}

// ------------------------------------------------------------------ report

/// `report`: each subject's section, plus the top table and the synthetic
/// headline, rebuilt from every subject that has results. Answers with the
/// document and how many subjects it rendered.
pub fn regenerate_report(
    session: &Session,
    subject: Option<&str>,
    doc: Option<&Path>,
) -> Result<(PathBuf, usize)> {
    let doc = doc
        .map(Path::to_path_buf)
        .unwrap_or_else(|| session.root.join("docs/RESULTS.md"));
    let parent = &session.results_root;
    // Every subject at once, not just the named one: a per-subject nightly job
    // rewrites the top table and the synthetic headline wholesale, and must not
    // drop its neighbours' rows on the way.
    let results = report::SubjectResults::collect(&session.manifest, parent, &session.subjects)?;
    anyhow::ensure!(
        !results.is_empty(),
        "report: no walk-*.json under {}",
        parent.display()
    );
    let only = subject.map(crate::session::subject_dir_name);
    if let Some(id) = &only {
        anyhow::ensure!(
            results.iter().any(|r| &r.id == id),
            "no results for {id} under {}",
            parent.display()
        );
    }
    for r in &results {
        if only.as_ref().is_some_and(|id| &r.id != id) {
            continue;
        }
        report::update_doc(&doc, &r.id, &report::render_subject_section(r))?;
    }
    report::update_doc(&doc, "top", &report::render_top_table(&results))?;
    tracing::info!(doc = %doc.display(), subjects = results.len(), "report written");
    Ok((doc, results.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::Tier;

    #[test]
    fn a_walk_summary_names_the_recall_and_the_verdict() {
        let mut r = crate::walk::WalkResult {
            schema: crate::RESULT_SCHEMA,
            repo: "envoy".into(),
            pin: "0123456789abcdef".into(),
            from: "a".into(),
            to: "0123456789abcdef".into(),
            tier: Tier::Correctness,
            schedule: Tier::Correctness.defaults(),
            oracle_sample: Default::default(),
            peer_sample: Default::default(),
            environment: crate::env::Environment {
                machine: crate::harness::Machine::detect(),
                bazel_version_used: String::new(),
                cambi_version: String::new(),
                tool_sha: String::new(),
                engine_id: String::new(),
                config_hash: String::new(),
                output_bases: Default::default(),
            },
            started_unix: 0,
            index_build_ms: 0,
            steps: Vec::new(),
            rebuild_oracle: None,
            head_index: None,
            summary: Default::default(),
            done: true,
        };
        r.summarise();
        let s = walk_summary(&r);
        assert!(s.contains("worst recall"), "{s}");
        assert!(s.contains("PASS"), "{s}");
    }
}
