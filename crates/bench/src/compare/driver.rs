//! Running the comparison: cambium, the tools and the oracles over one change
//! set, and over a set of them with resume.
//!
//! This is the only part of `compare` that touches a checkout, a Bazel or a
//! subprocess.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context as _, Result};
use cambium_bazel::Bazel;
use cambium_engine::Engine;

use crate::changes::ChangeSet;
use crate::checkout::Checkout;
use crate::classify::BazelFacts;
use crate::oracles::rdeps::{OracleInputs, RdepsOracle};
use crate::state::State;
use crate::tools::{BaseWorktree, RunContext, Tool};

use super::model::{
    absolute, CambiumRun, ChangeSetResult, ChangesOptions, ChangesResult, ChangesSummary,
    ToolComparison,
};
use super::summary::{classify_all, examples_of, precision};

/// A Bazel rooted at another worktree, carrying this run's startup flags and
/// environment.
///
/// Everything but `--output_base`: that one is per workspace directory, and
/// lending it to a second workspace corrupts repository rules that symlink
/// into the workspace they were fetched from. See
/// [`RunContext::tool_startup_flags`].
fn bazel_at(engine: &Engine, workspace: &Path) -> Bazel {
    let startup: Vec<String> = engine
        .bazel
        .startup_flags()
        .iter()
        .filter(|f| !f.starts_with("--output_base"))
        .cloned()
        .collect();
    Bazel::at(
        workspace.to_path_buf(),
        Some(&engine.bazel.binary().display().to_string()),
    )
    .with_startup_flags(startup)
    .with_env(engine.bazel.env().to_vec())
}

/// The run context tools are handed for one change set.
pub fn run_context(engine: &Engine, scratch: &Path, base_worktree: &Path) -> RunContext {
    RunContext {
        bazel_binary: engine.bazel.binary().to_path_buf(),
        startup_flags: engine.bazel.startup_flags().to_vec(),
        env: engine.bazel.env().to_vec(),
        scratch: absolute(scratch),
        base_worktree: absolute(base_worktree),
    }
}

/// How many of `labels` cambium reached for each `--explain` reason.
fn reason_histogram<'a>(
    labels: impl IntoIterator<Item = &'a String>,
    explanations: &[cambium_engine::report::Explanation],
) -> BTreeMap<String, usize> {
    let wanted: BTreeSet<&str> = labels.into_iter().map(String::as_str).collect();
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    for e in explanations {
        if wanted.contains(e.target.as_str()) {
            *out.entry(e.reason.describe()).or_default() += 1;
        }
    }
    out
}

/// The labels themselves plus every label on their `--explain` path: the set
/// the classifier will ask Bazel about, so it can be fetched in one batch.
fn with_paths<'a>(
    labels: impl IntoIterator<Item = &'a String>,
    explanations: &[cambium_engine::report::Explanation],
) -> Vec<String> {
    let wanted: BTreeSet<&str> = labels.into_iter().map(String::as_str).collect();
    let mut out: BTreeSet<String> = wanted.iter().map(|s| s.to_string()).collect();
    for e in explanations {
        if wanted.contains(e.target.as_str()) {
            out.extend(e.path.iter().cloned());
        }
    }
    out.into_iter().collect()
}

/// One change set, with defaults for scratch and the example cap.
pub fn compare_change_set(
    engine: &Engine,
    checkout: &Checkout,
    change_set: &ChangeSet,
    tools: &[&dyn Tool],
    oracle_k: usize,
    seed: u64,
) -> Result<ChangeSetResult> {
    let opts = ChangesOptions {
        oracle_k,
        seed,
        ..ChangesOptions::new(
            checkout.id.clone(),
            change_set.head.clone(),
            checkout.dir.join("bench-results"),
        )
    };
    compare_change_set_with(engine, checkout, change_set, tools, &opts)
}

/// One change set, with the run's options.
pub fn compare_change_set_with(
    engine: &Engine,
    checkout: &Checkout,
    change_set: &ChangeSet,
    tools: &[&dyn Tool],
    opts: &ChangesOptions,
) -> Result<ChangeSetResult> {
    let mut owned = opts.clone();
    owned.make_absolute();
    compare_one(engine, checkout, change_set, tools, &owned)
}

/// One change set, with options already made absolute.
///
/// Separate from [`compare_change_set_with`] because the loop in
/// [`compare_change_sets`] normalised the paths once and then cloned the whole
/// options struct again for every change set of the run.
fn compare_one(
    engine: &Engine,
    checkout: &Checkout,
    change_set: &ChangeSet,
    tools: &[&dyn Tool],
    opts: &ChangesOptions,
) -> Result<ChangeSetResult> {
    std::fs::create_dir_all(&opts.scratch)?;

    // ---- cambium, cold then warm ----
    //
    // The first run publishes whatever base index it had to compute, so the
    // second one finds it in the store: that is precisely the difference the
    // two numbers are meant to show.
    checkout.checkout(&change_set.head)?;
    let run =
        crate::tools::cambium::Cambium::new(engine).run(&change_set.base, &change_set.head)?;
    let (cold_wall_ms, warm_wall_ms) = (run.cold.as_millis() as u64, run.warm.as_millis() as u64);
    let out = run.out;

    // Keep the base this run actually spliced onto. `state_dir` is
    // `bench-results/<subject>/`, so the archive sits beside `state.json` and
    // the cold indexes every workload shares; `cambium-bench replay` looks for
    // it there. Without it the only base a later replay has is the index at
    // the pin, which is a different graph -- see `crate::replay`.
    if let Some(base) = out.base_index.as_ref() {
        if let Err(e) = archive_base_index(
            &opts.state_dir,
            &change_set.base,
            base,
            engine.index_compression(),
        ) {
            // A comparison that produced the right answer should not fail
            // because a diagnostic copy could not be written.
            tracing::warn!(base = %change_set.base, error = format!("{e:#}"), "could not archive the base index");
        }
    }

    let report = &out.report;
    let cambium_set: BTreeSet<String> = report.affected_labels().into_iter().collect();
    let cambium_depths: BTreeMap<String, u32> = report
        .affected
        .iter()
        .map(|t| (t.label.clone(), t.depth))
        .collect();
    let direct: Vec<String> = report.direct.clone();
    let cambium = CambiumRun {
        direct: report.direct.len(),
        affected: report.affected.len(),
        tests: report.tests.len(),
        global: report.global,
        global_reason: report.global_reason.clone(),
        cold_wall_ms,
        warm_wall_ms,
        dirty_packages: report.stats.dirty_packages,
        queried_targets: report.stats.queried_targets,
        changed_files: report.stats.changed_files,
        cold_rebuild: report.stats.cold_rebuild,
        universe: 0,
    };
    let Some(index) = out.index.as_ref() else {
        return compare_global(engine, checkout, change_set, tools, opts, report, cambium);
    };

    // The sets themselves, beside the JSON that keeps only their counts. Not
    // behind a flag and not best-effort: they are tens of kilobytes, and the
    // change set a run wants to explain afterwards is never the one somebody
    // thought to ask for them on. See [`crate::labels`].
    let labels_dir =
        crate::labels::change_set_dir(&opts.results_dir, &change_set.base, &change_set.head);
    std::fs::create_dir_all(&labels_dir)?;
    crate::labels::write_set(&labels_dir, crate::labels::CAMBIUM, &cambium_set)?;

    // ---- the facts a classification needs ----
    let base_wt = BaseWorktree::create(&checkout.dir, &change_set.base, &opts.scratch)?;
    let changed = BazelFacts::changed_paths(&checkout.dir, &change_set.base, &change_set.head)?;
    use cambium_engine::report::DirectReason;
    let reasoned = |f: fn(&DirectReason) -> bool| -> Vec<String> {
        report
            .explanations
            .iter()
            .flatten()
            .filter(|e| f(&e.reason))
            .map(|e| e.target.clone())
            .collect()
    };
    let paths: Vec<(String, Vec<String>)> = report
        .explanations
        .iter()
        .flatten()
        .map(|e| (e.target.clone(), e.path.clone()))
        .collect();
    let deleted_dep = reasoned(|r| matches!(r, DirectReason::DeletedDep { .. }));
    // Both ways a repository moves: because a main-repo file it is built from
    // (or `MODULE.bazel`) changed, and because a main-repo rule target its
    // generated targets depend on was reached by the walk. The second was
    // left out when the walk-time closure landed, and every target it added
    // came out `unclassified` — 165 on an envoy change set that only added a
    // changelog entry.
    let external_repo = reasoned(|r| {
        matches!(
            r,
            DirectReason::ExternalRepo { .. } | DirectReason::ExternalRepoTarget { .. }
        )
    });
    let from_build_file = reasoned(|r| match r {
        DirectReason::File { path } => cambium_classify::is_build_file(path),
        _ => false,
    });
    // ---- the oracle ----
    //
    // Before the tools, not after: its answer is one of the classifier's
    // facts. A cambium-side extra that no benign predicate explains is
    // `tool_missing` rather than `unclassified` when Bazel's own `rdeps`
    // reached the label too, and that verdict has to be available while the
    // difference is being classified.
    //
    // The base index is wanted only for the deleted half of the change set
    // (see `crate::oracles::rdeps`): a file that is gone at head has no label
    // to ask Bazel about, and the base index's `file -> consumers` rows are the
    // only record of who read it. The run above published it, so this is a
    // store read; when it is not there the oracle says so and checks the rest.
    let dirty_packages: BTreeSet<String> = report.dirty_packages.iter().cloned().collect();
    let oracle = match opts.rdeps_oracle {
        true => {
            let base_index = base_index_for_oracle(engine, &change_set.base);
            Some(
                RdepsOracle::new(opts.oracle_k, opts.seed)
                    .run(
                        &engine.bazel,
                        &OracleInputs {
                            head: index.view(),
                            base: base_index.as_ref().map(|f| f.view()),
                            changed: &changed,
                            cambium: &cambium_set,
                            dirty_packages: &dirty_packages,
                        },
                    )
                    .with_context(|| format!("rdeps oracle at {}", change_set.head))?,
            )
        }
        false => None,
    };
    let oracle_recall = oracle.as_ref().map(|o| o.recall()).unwrap_or(1.0);
    // Only when it ran: no file at all says "the oracle was off", which a
    // file holding an empty set would not.
    if let Some(o) = &oracle {
        crate::labels::write_set(&labels_dir, crate::labels::ORACLE, &o.oracle_set)?;
    }

    let facts = BazelFacts::new(
        &engine.bazel,
        index.view(),
        direct.iter().cloned(),
        changed.iter().cloned(),
    )
    .with_paths(paths)
    .with_affected(cambium_set.iter().cloned())
    .with_deleted_dep(deleted_dep)
    .with_external_repo(external_repo)
    .with_build_file(from_build_file)
    .with_base(bazel_at(engine, base_wt.path()));
    // Only when it ran: `with_oracle` on an empty answer means "Bazel reached
    // nothing", which `tool_extra_source_directory` may lean on, and a run
    // with `--oracle` off must not be able to say that.
    let facts = match &oracle {
        Some(o) => facts.with_oracle(o.oracle_set.iter().cloned()),
        None => facts,
    };

    // ---- the tools ----
    let ctx = run_context(engine, &opts.scratch, base_wt.path());
    let mut results = Vec::new();
    let mut rows: Vec<crate::labels::ClassRow> = Vec::new();
    for p in tools {
        let cmp = match p.affected(&checkout.dir, &change_set.base, &change_set.head, &ctx) {
            Ok(mut run) => {
                // Both tools report source-file targets (`//pkg:BUILD.bazel`
                // at distance 0); cambium's walk emits rules only. Comparing
                // the two vocabularies would file every such row as
                // `cambium_missing`, so the answer is restricted to the
                // universe cambium reports on and the drop is counted.
                run.restrict_to_rules(index.view());
                // One batched Bazel query for every label in the symmetric
                // difference, before the classifier asks about them one by
                // one; see `BazelFacts::prefetch`.
                facts.prefetch(with_paths(
                    cambium_set.symmetric_difference(&run.labels),
                    report.explanations.as_deref().unwrap_or_default(),
                ));
                // Restricted to rules first, so the stored set is the same
                // vocabulary the classification ran over: a file whose rows
                // the JSON's counts cannot be reconciled with is worse than
                // no file.
                crate::labels::write_set(&labels_dir, p.name(), &run.labels)?;
                let (counts, found) = classify_all(&cambium_set, &run.labels, &facts);
                rows.extend(found.iter().map(|d| crate::labels::ClassRow {
                    tool: p.name().to_string(),
                    side: d.side,
                    class: d.class,
                    label: d.label.clone(),
                }));
                let examples = examples_of(found, opts.examples);
                let failures: usize = counts
                    .iter()
                    .filter(|(class, _)| class.is_failure())
                    .map(|(_, n)| n)
                    .sum();
                ToolComparison {
                    labels: run.labels.len(),
                    cold_wall_ms: run.cold_wall.as_millis() as u64,
                    warm_wall_ms: run.warm_wall.map(|d| d.as_millis() as u64),
                    precision: precision(&run.labels, &cambium_set),
                    depth_agreement: run
                        .depths
                        .as_ref()
                        .and_then(|d| crate::tools::tango::depth_agreement(&cambium_depths, d)),
                    dropped_file_targets: run.dropped_file_targets,
                    cambium_only: cambium_set.difference(&run.labels).count(),
                    tool_only: run.labels.difference(&cambium_set).count(),
                    counts,
                    examples,
                    cambium_only_reasons: reason_histogram(
                        cambium_set.difference(&run.labels),
                        report.explanations.as_deref().unwrap_or_default(),
                    ),
                    pass: failures == 0,
                    ..ToolComparison::new(p.name(), p.version())
                }
            }
            Err(e) => {
                tracing::warn!(
                    tool = p.name(),
                    error = format!("{e:#}"),
                    "tool failed on this change set"
                );
                ToolComparison::failed(p.name(), p.version(), &e)
            }
        };
        results.push(cmp);
    }
    // Written once, with every tool's rows in it: which tool a label
    // disagreed with is a column, because the same label routinely disagrees
    // with one tool and not another.
    crate::labels::write_classes(&labels_dir, &rows)?;

    // ---- the `.bzl` load-closure oracle ----
    //
    // Only on change sets that changed a `.bzl`, which is where the static closure
    // is the thing under test. `engine.bazel` is the head checkout, which is
    // the tree `rbuildfiles` has to be asked about.
    let load_oracle = match opts.load_oracle {
        true => crate::oracles::load::run(
            &engine.bazel,
            &changed,
            &report.dirty_packages.iter().cloned().collect(),
        )
        .with_context(|| format!("bzl load-closure oracle at {}", change_set.head))?,
        false => None,
    };
    if let Some(o) = &load_oracle {
        if !o.repository_evaluation.is_empty() {
            tracing::info!(
                witnesses = ?o.repository_evaluation,
                named = o.bazel_packages.len(),
                checked = ?o.checked,
                "rbuildfiles named packages whose BUILD file loads nothing; \
                 this answer came through repository evaluation, not load edges"
            );
        } else if o.answered_by_repository_evaluation() && !o.missing_packages.is_empty() {
            tracing::info!(
                workspace_only = ?o.workspace_only,
                named = o.bazel_packages.len(),
                "rbuildfiles named packages for a .bzl no BUILD file loads; \
                 this answer came through repository evaluation, not load edges"
            );
        } else if !o.ok() {
            tracing::error!(
                missing = ?o.missing_packages,
                checked = ?o.checked,
                "rbuildfiles names packages the static load closure did not dirty"
            );
        }
    }

    let pass = results.iter().all(|p| p.pass)
        && oracle.as_ref().map_or(true, |o| o.ok())
        && load_oracle.as_ref().map_or(true, |o| o.ok());

    Ok(ChangeSetResult {
        schema: crate::RESULT_SCHEMA,
        base: change_set.base.clone(),
        head: change_set.head.clone(),
        kind: change_set.kind,
        category: change_set.category,
        cambium,
        tools: results,
        oracle,
        load_oracle,
        recall: oracle_recall,
        global: false,
        pass,
    })
}

/// The change set where cambium answered `//...`.
///
/// A globally invalidated change set (MODULE.bazel moved, a `.bazelrc` changed,
/// anything on the invalidator list) has no head index and no affected set:
/// cambium's answer is `//...`. There is nothing to classify — every label
/// any tool could name is trivially inside "everything" — and nothing
/// an oracle could disprove, so the change set enters no precision, class or
/// recall aggregate, is marked passing, and the run carries on.
///
/// The tools **do** run. Skipping them was the comfortable choice and
/// the dishonest one: it left the timing rows drawn over precisely the
/// change sets cambium is fast on, so the published speedup was an average over a
/// hand-picked half of the history. Here each tool pays its usual two
/// whole-repository query passes and the change set records what it cost, what
/// precise answer it found, and — from one `kind(rule, //...)` query — how
/// many targets cambium's `//...` is asking the user to build instead.
///
/// The two oracles do not run, and cannot:
///
/// - the rdeps oracle asks which rules the changed files reach and requires
///   cambium's answer to contain them; a global answer is `//...`, which
///   contains every rule by construction, so the check is vacuous and there
///   is no head index to seed it from anyway.
/// - the `.bzl` load-closure oracle checks `rbuildfiles` against the dirty
///   packages cambium named, and a global names none — running it would
///   report every package as missed, which is the opposite of the truth.
fn compare_global(
    engine: &Engine,
    checkout: &Checkout,
    change_set: &ChangeSet,
    tools: &[&dyn Tool],
    opts: &ChangesOptions,
    report: &cambium_engine::report::AffectedReport,
    cambium: CambiumRun,
) -> Result<ChangeSetResult> {
    let reason = report
        .global_reason
        .clone()
        .unwrap_or_else(|| "globally invalidated".to_string());
    tracing::info!(
        %reason,
        tools = tools.len(),
        "the change set is globally invalidated: cambium answers //..., and the tools run anyway so the cost of that is recorded"
    );
    // A failure here must not take the run down: the universe is a
    // reporting number, not a gate, and a repository whose `//...` will
    // not fully load still has a change set worth timing.
    let universe = match crate::oracles::rdeps::rule_universe(&engine.bazel) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(
                error = format!("{e:#}"),
                "could not count the rule universe; the global change set records 0"
            );
            BTreeSet::new()
        }
    };
    // Only when there is a tool to hand it to: a comparison run with
    // none configured must not pay for a worktree nobody reads.
    let base_wt = match tools.is_empty() {
        true => None,
        false => Some(BaseWorktree::create(
            &checkout.dir,
            &change_set.base,
            &opts.scratch,
        )?),
    };
    let ctx = run_context(
        engine,
        &opts.scratch,
        base_wt.as_ref().map_or(&checkout.dir, |w| w.path()),
    );
    let results: Vec<ToolComparison> = tools
        .iter()
        .map(
            |p| match p.affected(&checkout.dir, &change_set.base, &change_set.head, &ctx) {
                Ok(mut run) => {
                    run.restrict_to_rule_set(&universe);
                    ToolComparison {
                        labels: run.labels.len(),
                        cold_wall_ms: run.cold_wall.as_millis() as u64,
                        warm_wall_ms: run.warm_wall.map(|d| d.as_millis() as u64),
                        // Every label the tool named is inside `//...`, so
                        // there is nothing for cambium to have missed. It is
                        // recorded as 1.0 for the reader's sake and consumed
                        // by nothing: `summarise` takes no precision from a
                        // global change set.
                        precision: 1.0,
                        dropped_file_targets: run.dropped_file_targets,
                        ..ToolComparison::new(p.name(), p.version())
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        tool = p.name(),
                        error = format!("{e:#}"),
                        "tool failed on this global change set"
                    );
                    ToolComparison::failed(p.name(), p.version(), &e)
                }
            },
        )
        .collect();
    // cambium cannot be unsound on a global change set -- it answered
    // `//...` -- but a tool that could not run still fails it, exactly as
    // on an incremental one. Otherwise a subject whose every change set is
    // global reports a clean run against tools that never produced an
    // answer.
    let pass = results.iter().all(|t| t.pass);
    Ok(ChangeSetResult {
        schema: crate::RESULT_SCHEMA,
        base: change_set.base.clone(),
        head: change_set.head.clone(),
        kind: change_set.kind,
        category: change_set.category,
        cambium: CambiumRun {
            global: true,
            global_reason: Some(reason),
            universe: universe.len(),
            ..cambium
        },
        tools: results,
        oracle: None,
        load_oracle: None,
        recall: 1.0,
        global: true,
        pass,
    })
}

/// The base index, for the oracle's deleted half, or `None`.
///
/// Best effort by design: the oracle is a check, and a check that cannot be
/// made is reported ([`crate::oracles::rdeps::RdepsResult::deleted_checked`]),
/// not raised. The index is in the store because the run above published it;
/// a store that will not read, or a base whose index was never published,
/// costs the deleted half of one change set and nothing else.
fn base_index_for_oracle(engine: &Engine, base: &str) -> Option<cambium_index::IndexFile> {
    let rev = engine
        .repo
        .rev_parse(base)
        .unwrap_or_else(|_| base.to_string());
    let path = match engine.fetch_baseline(&rev) {
        Ok(p) => p?,
        Err(e) => {
            tracing::warn!(%base, error = format!("{e:#}"), "no base index for the oracle's deleted half");
            return None;
        }
    };
    match cambium_index::IndexFile::open(&path) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!(%base, error = format!("{e:#}"), "could not open the base index for the oracle");
            None
        }
    }
}

/// Copies the base index a run used to
/// `<results>/<subject>/bases/<base sha>.bin`, and says where it landed.
///
/// Two change sets in a row frequently share a base, and a base is megabytes,
/// so an existing file is left alone rather than rewritten: the store serves
/// one index per commit and it does not change under us within a run.
///
/// `compression` is the run's own (`baseline.compress_indexes`), so an
/// archived base is the same on-disk form as the `index-<pin>.bin` beside it
/// and `precheck.sh` pulls files of the size it expects. The copying itself
/// is [`crate::replay::archive_index`], shared with the history's head
/// archive.
pub fn archive_base_index(
    subject_results: &Path,
    base: &str,
    used: &cambium_engine::BaseIndex,
    compression: cambium_index::format::Compression,
) -> Result<Option<std::path::PathBuf>> {
    let dest = crate::replay::archived_base_path(subject_results, base);
    let src = match used {
        cambium_engine::BaseIndex::Mapped(p) => crate::replay::ArchiveSource::File(p),
        cambium_engine::BaseIndex::Owned(data) => crate::replay::ArchiveSource::Data(data),
    };
    let Some(dest) = crate::replay::archive_index(&dest, src, compression)? else {
        return Ok(None);
    };
    let bytes = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    tracing::info!(%base, path = %dest.display(), bytes, "archived the base index this change set ran against");
    Ok(Some(dest))
}

/// A previous file continues this run only when the same cambium wrote it:
/// the engine id names the index format and the engine configuration, and a
/// change set answered under another one is a different experiment.
pub(super) fn resumable(prev: &ChangesResult, opts: &ChangesOptions, engine_id: &str) -> bool {
    prev.repo == opts.repo_id && prev.pin == opts.pin && prev.engine == engine_id
}

pub(super) fn fresh_result(opts: &ChangesOptions, engine_id: &str) -> ChangesResult {
    ChangesResult {
        schema: crate::RESULT_SCHEMA,
        repo: opts.repo_id.clone(),
        pin: opts.pin.clone(),
        bazel_version: opts.bazel_version.clone(),
        bazel_version_used: None,
        seed: opts.seed,
        oracle_k: opts.oracle_k,
        engine: engine_id.to_string(),
        tango_change_sets: opts.tango_change_sets,
        change_sets: Vec::new(),
        summary: ChangesSummary::default(),
    }
}

/// Every change set, resumable, writing after each one.
///
/// A change set that errors outright (a tool that cannot run at all, a base
/// revision Bazel refuses to load) stops the run rather than being skipped: a
/// comparison that silently drops the hard change sets is worse than no comparison.
pub fn compare_change_sets(
    engine: &Engine,
    checkout: &Checkout,
    change_sets: &[ChangeSet],
    tools: &[&dyn Tool],
    opts: &ChangesOptions,
) -> Result<ChangesResult> {
    let mut owned = opts.clone();
    owned.make_absolute();
    let opts = &owned;
    let path = opts.output_path();
    let engine_id = State::engine_id(&engine.cfg.hash());
    let mut state = State::load_for(&opts.state_dir, &opts.repo_id, &opts.pin, &engine_id)?;
    let mut result = match (opts.resume, ChangesResult::read(&path)) {
        (true, Ok(prev)) if resumable(&prev, opts, &engine_id) => prev,
        (true, Ok(prev)) if prev.repo == opts.repo_id && prev.pin == opts.pin => {
            tracing::warn!(
                was = %prev.engine,
                now = %engine_id,
                "change sets were compared by a different cambium; starting over"
            );
            fresh_result(opts, &engine_id)
        }
        _ => fresh_result(opts, &engine_id),
    };
    // Recorded once per run, and re-recorded on a resume whose earlier half
    // predates the field: the report's numbers are only worth reading if they
    // name the Bazel that actually answered.
    if result.bazel_version_used.is_none() {
        let v = engine.bazel.version();
        let v = v.lines().next().unwrap_or("").trim().to_string();
        if !v.is_empty() {
            result.bazel_version_used = Some(v);
        }
    }
    let done: BTreeSet<String> = result.change_sets.iter().map(|p| p.key()).collect();

    for (i, change_set) in change_sets.iter().enumerate() {
        let key = change_set.key();
        if opts.resume && done.contains(&key) {
            tracing::debug!(%key, "already compared");
            continue;
        }
        // The cap is on the *change set index*, not on how many Tango has managed so
        // far, so which change sets it runs on is the same set whether the run
        // finished in one go or resumed at change set 30.
        let running: Vec<&dyn Tool> = tools
            .iter()
            .copied()
            .filter(|p| {
                p.name() != crate::tools::tango::NAME
                    || opts.tango_change_sets.map_or(true, |n| i < n)
            })
            .collect();
        tracing::info!(
            change_set = i + 1,
            of = change_sets.len(),
            %key,
            kind = change_set.kind.name(),
            tools = running.len(),
            "comparing"
        );
        let cmp = compare_one(engine, checkout, change_set, &running, opts)?;
        if !cmp.pass {
            tracing::warn!(%key, "change set failed the three-way comparison");
        }
        result.change_sets.push(cmp);
        result.summarise();
        result.write(&path)?;
        state.compared.insert(key);
        state.save(&opts.state_dir)?;
    }
    result.summarise();
    result.write(&path)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn an_index(commit: &str) -> cambium_index::IndexData {
        let mut b = cambium_index::IndexBuilder::new(cambium_index::new_lineage());
        b.apply_packages(&[cambium_index::PackageUpdate {
            package: "lib".into(),
            targets: vec![cambium_index::TargetRow::rule(
                "//lib:core",
                "cc_library",
                vec![],
            )],
            ..Default::default()
        }]);
        b.finish(commit)
    }

    /// A base the run computed exists only in memory, so it is written out;
    /// one the store served is copied. Either way what lands is an index a
    /// replay can open at the change set's own base.
    #[test]
    fn a_base_index_is_archived_under_its_own_sha_in_either_form() {
        let dir = tempfile::tempdir().unwrap();
        let results = dir.path().join("envoy");
        let compression = cambium_index::format::Compression::default();

        let owned = an_index("base-a");
        let at = archive_base_index(
            &results,
            "base-a",
            &cambium_engine::BaseIndex::Owned(Box::new(owned)),
            compression,
        )
        .unwrap()
        .expect("nothing was archived");
        assert_eq!(at, crate::replay::archived_base_path(&results, "base-a"));
        assert_eq!(
            cambium_index::IndexFile::open(&at).unwrap().view().commit,
            "base-a"
        );

        let served = dir.path().join("store-copy.bin");
        cambium_index::format::write_index(&served, &an_index("base-b")).unwrap();
        let at = archive_base_index(
            &results,
            "base-b",
            &cambium_engine::BaseIndex::Mapped(served),
            compression,
        )
        .unwrap()
        .expect("nothing was archived");
        assert_eq!(
            cambium_index::IndexFile::open(&at).unwrap().view().commit,
            "base-b"
        );
    }

    /// Consecutive change sets share a base and a base is megabytes, so the
    /// second one leaves the first's file alone rather than rewriting it.
    #[test]
    fn an_already_archived_base_is_not_written_again() {
        let dir = tempfile::tempdir().unwrap();
        let results = dir.path().join("envoy");
        let owned = cambium_engine::BaseIndex::Owned(Box::new(an_index("base-a")));
        let compression = cambium_index::format::Compression::default();
        let at = archive_base_index(&results, "base-a", &owned, compression)
            .unwrap()
            .unwrap();
        let written = std::fs::metadata(&at).unwrap().modified().unwrap();

        assert!(archive_base_index(&results, "base-a", &owned, compression)
            .unwrap()
            .is_none());
        assert_eq!(std::fs::metadata(&at).unwrap().modified().unwrap(), written);
    }
}
