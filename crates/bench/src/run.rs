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

use crate::changes::{self, ChangeSetKind, ChangeSetSet};
use crate::compare::ChangesResult;
use crate::history::{self, HistoryResult};
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

// ----------------------------------------------------------------- history

/// What `history` was asked to do.
#[derive(Clone, Debug, Default)]
pub struct HistoryRequest {
    /// `--commits`; the subject's window (or `--quick`'s) when absent.
    pub commits: Option<usize>,
    /// `--oracle`: `rebuild`, or `none`.
    pub oracles: Vec<String>,
    pub on_existing: OnExisting,
}

/// The history workload. Answers with the result and where it was written;
/// a failed rebuild oracle is in the result, not an error, so the binary can
/// print the path before it fails.
pub fn run_history(run: &SubjectRun<'_>, req: &HistoryRequest) -> Result<(PathBuf, HistoryResult)> {
    let rebuild_oracle = parse_history_oracles(&req.oracles)?;
    let commits = req
        .commits
        .or(run.session.quick.then_some(crate::QUICK_COMMITS));
    let results = run.workload("history")?;
    let path = HistoryResult::path_versioned(&results, &run.entry.pin, run.version());
    refuse_existing(&path, "history", req.on_existing, || {
        HistoryResult::read(&path).ok().map(|h| h.commits.len())
    })?;
    let checkout = run.checkout()?;
    let engine = checkout.engine(&run.entry, run.version())?;
    let _servers = crate::servers::StopOnDrop::subject(
        &checkout.dir,
        &run.results.join("scratch"),
        engine.bazel().startup_flags(),
    );
    let cambi = run.cambi(&checkout)?;
    let range = commits.unwrap_or(run.entry.history_window);
    let commits = changes::range_commits(&checkout.repo, &run.entry.pin, range)?;
    tracing::info!(
        commits = commits.len(),
        from = %commits.first().cloned().unwrap_or_default(),
        to = %run.entry.pin,
        "running the history"
    );
    let progress = run.progress("history", Step::History, Some(cambi.version().to_string()));
    let opts = history::HistoryOptions {
        repo_id: run.entry.id.clone(),
        pin: run.entry.pin.clone(),
        results_dir: results.clone(),
        state_dir: run.results.clone(),
        publish: true,
        resume: req.on_existing == OnExisting::Resume,
        bazel_version: run.version().unwrap_or_default().to_string(),
        diff_cap: 50,
        rebuild_oracle,
        cambi_version: Some(cambi.version().to_string()),
        progress: Some(progress.clone()),
    };
    let driver = history::CambiDriver {
        checkout: &checkout,
        cambi: &cambi,
    };
    let result = history::history(&engine, &driver, &commits, &opts)?;
    tracing::info!(
        commits = result.summary.commits,
        p50_ms = result.summary.p50_total_ms,
        p90_ms = result.summary.p90_total_ms,
        globals = result.summary.globals,
        cold_rebuilds = result.summary.engine_cold_rebuilds,
        oracle_ok = result.oracle.as_ref().map(|o| o.ok).unwrap_or(false),
        partial = result.cold.partial.partial,
        packages_in_error = result.cold.partial.packages_in_error,
        path = %path.display(),
        "history done"
    );
    Ok((path, result))
}

/// `rebuild`, or `none`.
pub fn parse_history_oracles(oracles: &[String]) -> Result<bool> {
    let mut on = false;
    for o in oracles {
        match o.trim() {
            "rebuild" => on = true,
            "none" | "" => {}
            other => anyhow::bail!("unknown history oracle {other:?}: rebuild, none"),
        }
    }
    Ok(on)
}

// -------------------------------------------------------------- change sets

/// How `changes` selects its change sets.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    /// `--kind`; every kind when empty.
    pub kinds: Vec<String>,
    /// How many change sets in total.
    pub cap: usize,
    /// Stress change sets per category.
    pub stress_cap: usize,
}

/// Selects the change sets and records them under `<subject>/changes/`.
///
/// The one selector for both `--select-only` and a comparison, so both draw
/// the stress change sets from the same hub list.
pub fn select_change_sets(
    run: &SubjectRun<'_>,
    sel: &Selection,
) -> Result<(PathBuf, ChangeSetSet)> {
    let explicit = !sel.kinds.is_empty();
    let kinds = parse_kinds(&sel.kinds)?;
    let checkout = run.checkout()?;
    let (paths, globs) = run.entry.invalidator_rules();
    let opts = changes::ChangeSetOptions {
        repo_id: run.entry.id.clone(),
        pin: run.entry.pin.clone(),
        history_window: run.entry.history_window,
        seed: run.session.seed,
        cap: sel.stress_cap,
        total_cap: Some(sel.cap),
        // Only when a stress change set is actually wanted: the hubs cost an
        // index open, and every other kind ignores them.
        //
        // Best effort when `stress` came from the default set of kinds rather
        // than from a `--kind stress` the person typed: three of the four
        // stress categories need no index at all, and refusing to select any
        // of them because `subjects build` has not run yet would make the
        // default `changes` unrunnable on a fresh subject. Asking for stress
        // explicitly and silently getting no hub change sets is the other
        // failure, so that one is an error.
        hub_packages: match kinds.contains(&ChangeSetKind::Stress) {
            false => Vec::new(),
            true if !explicit => run.hub_packages().unwrap_or_else(|e| {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "no hub packages; the stress selection covers every category but `hub`"
                );
                Vec::new()
            }),
            true => run.hub_packages()?,
        },
        invalidator_paths: paths,
        invalidator_globs: globs,
    };
    let set = changes::select(&checkout.repo, &kinds, &opts)?;
    run.workload("changes")?;
    let path = run.change_sets_path();
    set.write(&path)?;
    tracing::info!(change_sets = set.change_sets.len(), path = %path.display(), "selected the change sets");
    Ok((path, set))
}

/// `pr`, `adjacent`, `stress`; all three when nothing is named.
pub fn parse_kinds(kinds: &[String]) -> Result<Vec<ChangeSetKind>> {
    if kinds.is_empty() {
        return Ok(vec![
            ChangeSetKind::Pr,
            ChangeSetKind::Adjacent,
            ChangeSetKind::Stress,
        ]);
    }
    kinds
        .iter()
        .map(|k| {
            ChangeSetKind::parse(k).ok_or_else(|| {
                anyhow::anyhow!("unknown change-set kind {k:?}: pr, adjacent, stress")
            })
        })
        .collect()
}

/// The size `changes` runs at: `--change-sets`, else `--quick`'s, else the
/// default.
pub fn change_set_cap(session: &Session, explicit: Option<usize>) -> usize {
    explicit
        .or(session.quick.then_some(crate::QUICK_CHANGE_SETS))
        .unwrap_or(crate::DEFAULT_CHANGE_SETS)
}

// ----------------------------------------------------------------- compare

/// What `changes` was asked to do.
#[derive(Clone, Debug, Default)]
pub struct CompareRequest {
    /// `--change-set-file`: a selection to run instead of making one.
    pub change_set_file: Option<PathBuf>,
    pub selection: Selection,
    /// `--oracle-sample`.
    pub oracle_sample: usize,
    /// `--oracle`: `rdeps`, `load`, `none`.
    pub oracles: Vec<String>,
    /// `--tools`, as typed.
    pub tools: Vec<String>,
    pub on_existing: OnExisting,
}

/// What a comparison answered with.
#[derive(Clone, Debug)]
pub struct Compared {
    pub result: ChangesResult,
    pub path: PathBuf,
}

/// The change-set workload: every tool on every change set, scored by the
/// oracles. Errors when no named tool is installed — a run that compares
/// against nothing has measured nothing.
pub fn compare(
    run: &SubjectRun<'_>,
    req: &CompareRequest,
    locations: &ToolLocations,
) -> Result<Compared> {
    let wanted = tools::wanted(&req.tools)?;
    let owned: Vec<Box<dyn tools::Tool>> = wanted
        .iter()
        .map(|n| tools::open(n, locations))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    anyhow::ensure!(
        !owned.is_empty() || wanted.is_empty(),
        "changes: no tool is installed; nothing to compare against"
    );
    let tool_refs: Vec<&dyn tools::Tool> = owned.iter().map(|p| p.as_ref()).collect();

    let checkout = run.checkout()?;
    let set = load_change_sets(run, req)?;
    let change_sets: Vec<_> = set
        .change_sets
        .iter()
        .take(req.selection.cap.max(1))
        .cloned()
        .collect();
    anyhow::ensure!(!change_sets.is_empty(), "no change sets to compare");

    let engine = checkout.engine(&run.entry, run.version())?;
    let _servers = crate::servers::StopOnDrop::subject(
        &checkout.dir,
        &run.results.join("scratch"),
        engine.bazel().startup_flags(),
    );
    let (rdeps, load) = parse_change_oracles(&req.oracles)?;
    let results = run.workload("changes")?;
    let cambi = run.cambi(&checkout)?;
    let progress = run.progress("changes", Step::Changes, Some(cambi.version().to_string()));
    let opts = crate::compare::ChangesOptions {
        bazel_version: run.version().map(str::to_string),
        progress: Some(progress),
        rdeps_oracle: rdeps,
        oracle_k: req.oracle_sample,
        load_oracle: load,
        seed: run.session.seed,
        resume: req.on_existing == OnExisting::Resume,
        scratch: run.results.join("scratch"),
        state_dir: run.results.clone(),
        tango_change_sets: run.session.quick.then_some(crate::QUICK_TANGO_CHANGE_SETS),
        ..crate::compare::ChangesOptions::new(run.entry.id.clone(), run.entry.pin.clone(), results)
    };
    let output = opts.output_path();
    refuse_existing(&output, "comparison", req.on_existing, || {
        ChangesResult::read(&output)
            .ok()
            .map(|c| c.change_sets.len())
    })?;
    tracing::info!(
        change_sets = change_sets.len(),
        tools = tool_refs.len(),
        oracle_k = opts.oracle_k,
        tango_change_sets = opts.tango_change_sets.unwrap_or(change_sets.len()),
        "comparing"
    );
    let result = crate::compare::compare_change_sets(
        &engine,
        &cambi,
        &checkout,
        &change_sets,
        &tool_refs,
        &opts,
    )?;
    Ok(Compared {
        result,
        path: opts.output_path(),
    })
}

/// The change sets to compare: an explicit file, else the one a previous run
/// wrote for this pin, else a fresh selection with the requested kinds.
fn load_change_sets(run: &SubjectRun<'_>, req: &CompareRequest) -> Result<ChangeSetSet> {
    if let Some(p) = &req.change_set_file {
        return ChangeSetSet::read(p).with_context(|| format!("reading {}", p.display()));
    }
    let default = run.change_sets_path();
    if req.selection.kinds.is_empty() && default.exists() {
        tracing::info!(path = %default.display(), "using the recorded change sets");
        return ChangeSetSet::read(&default);
    }
    Ok(select_change_sets(run, &req.selection)?.1)
}

/// `rdeps` and `load`, in any order, or `none`.
pub fn parse_change_oracles(oracles: &[String]) -> Result<(bool, bool)> {
    let (mut rdeps, mut load) = (false, false);
    for o in oracles {
        match o.trim() {
            "rdeps" => rdeps = true,
            "load" => load = true,
            "none" | "" => {}
            other => anyhow::bail!("unknown oracle {other:?}: rdeps, load, none"),
        }
    }
    Ok((rdeps, load))
}

/// The table a person reads before opening the JSON.
pub fn compare_summary(result: &ChangesResult) -> String {
    use std::fmt::Write as _;
    let s = &result.summary;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\n{} {}  change sets {}  passed {}  global {}  min recall {:.1}%",
        result.repo,
        &result.pin[..result.pin.len().min(12)],
        s.change_sets,
        s.passed,
        s.globals,
        s.min_recall * 100.0
    );
    let _ = writeln!(
        out,
        "{:<20} {:>10} {:>12} {:>12}",
        "tool", "precision", "cold p50", "warm p50"
    );
    for (name, p) in &s.mean_precision {
        let cold = s.tool_cold.get(name).map(|w| w.p50_ms).unwrap_or(0);
        let warm = s
            .tool_warm
            .get(name)
            .map(|w| format!("{} ms", w.p50_ms))
            .unwrap_or_else(|| "—".into());
        let _ = writeln!(
            out,
            "{name:<20} {:>9.1}% {:>9} ms {warm:>12}",
            p * 100.0,
            cold
        );
    }
    let _ = writeln!(
        out,
        "{:<20} {:>10} {:>9} ms {:>9} ms",
        "cambium", "—", s.cambium_cold.p50_ms, s.cambium_warm.p50_ms
    );
    let failing: Vec<_> = s
        .by_class
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(k, n)| format!("{k}={n}"))
        .collect();
    let _ = write!(
        out,
        "disagreements: {}",
        if failing.is_empty() {
            "none".to_string()
        } else {
            failing.join(" ")
        }
    );
    out
}

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
    let dir = labels::resolve(&run.workload_path("changes"), &q.change_set)?;
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
        "report: no history-*.json or changes-*.json under {}",
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
    let bench = report::BenchLegacy::read(&session.root.join("docs/benchmarks/sweep-legacy.json"));
    for r in &results {
        if only.as_ref().is_some_and(|id| &r.id != id) {
            continue;
        }
        report::update_doc(&doc, &r.id, &report::render_subject_section(r, None))?;
    }
    report::update_doc(&doc, "top", &report::render_top_table(&results))?;
    report::update_doc(
        &doc,
        "synthetic",
        &report::render_bench_section(&bench, Some("bench-legacy.svg")),
    )?;
    tracing::info!(doc = %doc.display(), subjects = results.len(), "report written");
    Ok((doc, results.len()))
}

// ---------------------------------------------------------- stage benches

/// `bench-results/<subject>/<workload>/` for a stage benchmark, which needs
/// no checkout and no manifest entry: a synthetic preset is enough.
pub fn stage_dir(session: &Session, subject: &str, workload: &str) -> Result<PathBuf> {
    let dir = session
        .results_root
        .join(crate::session::subject_dir_name(subject))
        .join(workload);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The preset name behind a `synth:<preset>` subject, for the stage
/// benchmarks — which time a generated graph directly and never go near a
/// workspace.
pub fn preset_of(subject: &str, what: &str) -> Result<String> {
    subject
        .strip_prefix(crate::subject::SYNTH_PREFIX)
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "`cambium-bench {what} {subject}`: {what} runs on a synthetic subject \
                 (synth:<preset>); a real subject has no generated graph to \
                 measure a stage over"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_oracle_lists_parse() {
        assert!(parse_history_oracles(&["rebuild".into()]).unwrap());
        assert!(!parse_history_oracles(&["none".into()]).unwrap());
        assert!(parse_history_oracles(&["x".into()]).is_err());
        assert_eq!(
            parse_change_oracles(&["load".into(), "rdeps".into()]).unwrap(),
            (true, true)
        );
        assert_eq!(
            parse_change_oracles(&["none".into()]).unwrap(),
            (false, false)
        );
        assert!(parse_change_oracles(&["bogus".into()]).is_err());
    }

    #[test]
    fn the_kinds_default_to_all_three() {
        assert_eq!(parse_kinds(&[]).unwrap().len(), 3);
        assert_eq!(
            parse_kinds(&["stress".into()]).unwrap(),
            [ChangeSetKind::Stress]
        );
        assert!(parse_kinds(&["nope".into()]).is_err());
        assert!(preset_of("synth:small", "stages").unwrap() == "small");
        assert!(preset_of("bazel", "stages").is_err());
    }

    #[test]
    fn the_summary_names_every_tool_and_the_disagreements() {
        use crate::compare::{ChangesSummary, WallStats};
        let mut summary = ChangesSummary {
            change_sets: 3,
            passed: 2,
            min_recall: 0.5,
            ..Default::default()
        };
        summary.mean_precision.insert("tango".into(), 0.75);
        summary.tool_cold.insert(
            "tango".into(),
            WallStats {
                p50_ms: 1200,
                ..Default::default()
            },
        );
        summary
            .by_class
            .insert(crate::classify::Class::CambiumMissing, 4);
        let result = ChangesResult {
            cambi_version: None,
            schema: crate::RESULT_SCHEMA,
            repo: "bazel".into(),
            pin: "0123456789abcdef".into(),
            bazel_version: None,
            bazel_version_used: None,
            seed: 0,
            oracle_k: 0,
            engine: String::new(),
            tango_change_sets: None,
            change_sets: Vec::new(),
            summary,
        };
        let s = compare_summary(&result);
        assert!(
            s.contains("bazel 0123456789ab  change sets 3  passed 2"),
            "{s}"
        );
        assert!(
            s.contains("tango                     75.0%      1200 ms            —"),
            "{s}"
        );
        assert!(s.ends_with("disagreements: cambium_missing=4"), "{s}");
    }
}
