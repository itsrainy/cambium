//! `cambium-bench replay` — the engine's static stages, offline, against a
//! published run's own artefacts.
//!
//! It lives in the harness because everything it reads is a benchmark artefact:
//! the index at the pin, the recorded change sets and the recorded answers.
//! It runs no Bazel and needs no checkout, so it is the check to run *before*
//! the benchmark machine is started, not after.
//!
//! The orchestration is [`cambium_bench::precheck`]; this file is the flags
//! and the rendering. See [`cambium_bench::replay`] for what a replay can and
//! cannot know; the rendering repeats the important half in the output,
//! because a number whose limits are only in a doc comment will be quoted
//! without them.

use std::path::PathBuf;

use anyhow::Result;
use cambium_bench::precheck::{self, ReplayReport, ReplayRequest};
use cambium_bench::replay::{self, PackageRule, RecordedAnswer, ReplayOutcome, ReplayRun};
use clap::Args as ClapArgs;

use crate::Context;
use crate::DataFormat;

#[derive(ClapArgs, Clone)]
pub struct Args {
    /// A subject from subject_repos.toml.
    pub subject: String,

    /// Repo-relative paths to replay, as `git diff --name-status` spells them
    /// (`M<TAB>path`) or bare (read as a modification).
    #[arg(long = "paths", value_delimiter = ',')]
    pub paths: Vec<String>,
    /// A recorded change set, as `base..head`. Short revisions are fine.
    #[arg(long = "change-set")]
    pub change_set: Option<String>,
    /// A recorded history commit, by sha. Its parent is the base.
    #[arg(long = "commit")]
    pub commit: Option<String>,
    /// Every recorded change set of the subject.
    #[arg(long)]
    pub all: bool,
    /// With --all, also replay the recorded history commits. Off by default:
    /// a history commit's base is the index the previous commit left behind,
    /// not the pin, so a divergence there is usually drift rather than a bug.
    #[arg(long)]
    pub history: bool,

    /// The index to replay against, overriding everything: neither the
    /// archived base of a change set nor the index at the pin is consulted.
    /// Default: the change set's own base index when the run archived one at
    /// <results-dir>/<subject>/bases/<base>.bin, else
    /// <results-dir>/<subject>/index-<pin>.bin.
    #[arg(long)]
    pub index: Option<PathBuf>,
    #[command(flatten)]
    pub common: crate::bench::Common,

    /// A cached diff per change set, written by `--write-paths` on the box
    /// and pulled by `cambium-bench pull-results`. The recorded change sets
    /// carry `(base, head)` and no paths, so without either this or --repo
    /// there is nothing to replay.
    #[arg(long = "paths-file")]
    pub paths_file: Option<PathBuf>,
    /// A git repository (a clone or the subject's mirror) to take the diff
    /// from, and to read MODULE.bazel at either revision. With it a replay
    /// decides the change sets it otherwise marks indeterminate.
    #[arg(long = "repo")]
    pub repo: Option<PathBuf>,
    /// Write the diff of every recorded change set — the paths cache a
    /// replay away from --repo reads — and replay nothing. `-` prints it.
    /// Default: <results-dir>/<subject>/paths-<pin>.json.
    #[arg(long = "write-paths", value_name = "FILE", num_args = 0..=1, default_missing_value = "")]
    pub write_paths: Option<String>,

    /// Rebuild the externals side tables from the repository text at each
    /// change set's base before replaying it, instead of reading back the
    /// tables the run's cambium derived. This is what makes an index-time
    /// change — `build_repo_inputs` and what feeds it — visible offline.
    /// Requires --repo. A re-derived replay is not comparable against a plain
    /// one as engine evidence; --baseline says which side was which.
    #[arg(long)]
    pub rederive: bool,
    /// With --rederive, keep each re-derived index at
    /// <DIR>/rederived-<base>.bin for `cambi inspect externals`.
    #[arg(long, value_name = "DIR", env = "CAMBIUM_REDERIVE_DUMP")]
    pub rederive_dump: Option<PathBuf>,

    /// Check the *rebuild oracle's* externals half offline, instead of
    /// replaying change sets: open the index Bazel cold-built at the pin
    /// (`<results-dir>/<subject>/heads/<pin>.bin`, archived by the history's
    /// rebuild oracle), re-derive its externals side tables from the head
    /// tree's own text with this build, and compare the two by label. Where
    /// --rederive can only hold this build's derivation against what an
    /// earlier one recorded, this holds it against Bazel's truth. Requires
    /// --repo, and is silent about a subject whose run archived no head
    /// index.
    #[arg(long = "head-check")]
    pub head_check: bool,

    /// Compare against a previous `--all --save-baseline` file rather than
    /// against what the run recorded. This is the tight loop: both sides ran
    /// on the same stored index, so every difference is the engine's.
    #[arg(long)]
    pub baseline: Option<PathBuf>,
    /// Write this run's replays here, to be a later run's --baseline.
    #[arg(long = "save-baseline")]
    pub save_baseline: Option<PathBuf>,

    /// How many dirty packages and repositories to list per change set.
    #[arg(long, default_value_t = 20)]
    pub max_listed: usize,
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    let session = ctx.session(&args.common)?;
    let run = session.subject(&args.subject)?;
    if let Some(out) = &args.write_paths {
        let repo = args.repo.as_deref().ok_or_else(|| {
            anyhow::anyhow!("--write-paths takes the diffs from --repo <git dir>")
        })?;
        let dest = match out.as_str() {
            "" => None,
            "-" => Some(PathBuf::from("/dev/stdout")),
            p => Some(PathBuf::from(p)),
        };
        let written = precheck::write_paths_cache(&run, repo, dest.as_deref())?;
        if out != "-" {
            eprintln!("cambium-bench replay: wrote {}", written.display());
        }
        return Ok(crate::EXIT_OK);
    }
    if args.head_check {
        return head_check(ctx, &args, &run);
    }
    let req = ReplayRequest {
        paths: args.paths.clone(),
        change_set: args.change_set.clone(),
        commit: args.commit.clone(),
        all: args.all,
        history: args.history,
        index: args.index.clone(),
        paths_file: args.paths_file.clone(),
        repo: args.repo.clone(),
        rederive: args.rederive,
        rederive_dump: args.rederive_dump.clone(),
        baseline: args.baseline.clone(),
        save_baseline: args.save_baseline.clone(),
    };
    let report = precheck::replay_subject(&run, &req)?;
    match ctx.format.data() {
        None => {}
        Some(DataFormat::Json) | Some(DataFormat::Jsonl) => {
            println!("{}", serde_json::to_string_pretty(&report.json())?);
        }
        Some(DataFormat::Text) => render(&report, &args),
    }
    Ok(if report.clean() {
        crate::EXIT_OK
    } else {
        crate::EXIT_ERROR
    })
}

fn render(report: &ReplayReport, args: &Args) {
    if args.all || report.run.outcomes.len() > 1 {
        render_all(report);
        return;
    }
    for o in &report.run.outcomes {
        render_one(
            o,
            replay::find_recorded(&report.recorded, &o.base, &o.head),
            args.max_listed,
        );
    }
    for d in &report.divergences {
        println!("DIVERGENCE {d}");
    }
}

/// `--head-check`: the rebuild oracle's externals half, offline.
///
/// The nodes and the edges are the archived index's on both sides by
/// construction, so an empty graph diff proves nothing and the externals
/// section is the whole signal — which is why the summary line counts
/// externals mismatches and mentions the rest only when there are any (there
/// should never be, and a count here is a bug in the re-derivation rather
/// than in the derivation under test).
fn head_check(
    ctx: &Context,
    args: &Args,
    run: &cambium_bench::session::SubjectRun<'_>,
) -> Result<u8> {
    let Some((path, diff)) =
        precheck::head_check_subject(run, args.repo.as_deref(), args.max_listed)?
    else {
        return Ok(crate::EXIT_OK);
    };
    let (subject, pin) = (&run.entry.id, &run.entry.pin);
    match ctx.format.data() {
        None => {}
        Some(DataFormat::Json) | Some(DataFormat::Jsonl) => {
            let doc = serde_json::json!({
                "schema": replay::REPLAY_SCHEMA,
                "subject": subject,
                "pin": pin,
                "head_index": path.display().to_string(),
                "head_check": diff,
            });
            println!("{}", serde_json::to_string_pretty(&doc)?);
        }
        Some(DataFormat::Text) => {
            println!(
                "{subject}: head check at {pin}: {} externals mismatches",
                diff.counts.externals_mismatch
            );
            for line in &diff.externals_mismatch {
                println!("    {line}");
            }
            if diff.counts.externals_mismatch > diff.externals_mismatch.len() {
                println!(
                    "    ... {} more",
                    diff.counts.externals_mismatch - diff.externals_mismatch.len()
                );
            }
            if diff.counts.sky_mismatch > 0 {
                println!(
                    "{subject}: head check at {pin}: {} loading layer mismatches",
                    diff.counts.sky_mismatch
                );
                for line in &diff.sky_mismatch {
                    println!("    {line}");
                }
                if diff.counts.sky_mismatch > diff.sky_mismatch.len() {
                    println!(
                        "    ... {} more",
                        diff.counts.sky_mismatch - diff.sky_mismatch.len()
                    );
                }
            }
            let mut d = diff.clone();
            d.counts.externals_mismatch = 0;
            d.counts.sky_mismatch = 0;
            if !d.is_empty() {
                println!(
                    "    the graph itself differs, which a re-derivation cannot cause: {}",
                    d.summary()
                );
            }
        }
    }
    Ok(if diff.is_empty() {
        crate::EXIT_OK
    } else {
        crate::EXIT_ERROR
    })
}

fn render_one(o: &ReplayOutcome, recorded: Option<&RecordedAnswer>, max: usize) {
    println!("{} .. {}", o.base, o.head);
    println!("  changed files:   {}", o.changed_files);
    println!(
        "  base index:      {}",
        match o.base_kind {
            replay::BaseKind::RunsOwn => format!("the run's own (bases/{}.bin)", o.base),
            replay::BaseKind::Pin => "pin".to_string(),
            replay::BaseKind::Explicit => "--index".to_string(),
        }
    );
    if o.rederived {
        println!(
            "  REDERIVED:       the externals tables were rebuilt from the repository text at \
             {}{}",
            o.base,
            match o.rederive_ms {
                Some(ms) => format!(" ({ms} ms)"),
                None => " (cached from an earlier change set on this base)".into(),
            }
        );
    }
    for note in &o.indeterminate {
        println!("  INDETERMINATE:   {note}");
    }
    if o.global {
        println!(
            "  global:          yes — {}",
            o.global_reason.as_deref().unwrap_or("(no reason recorded)")
        );
    } else {
        println!("  global:          no");
    }
    println!("  repositories dirtied: {}", o.repos.len());
    for r in o.repos.iter().take(max) {
        let rule = match r.rule {
            replay::RepoRule::InputPath => "input path",
            replay::RepoRule::TargetOnly => "target-only",
            replay::RepoRule::ModuleDiff => "module diff",
            replay::RepoRule::Load => "load",
        };
        let loads = if r.loads_too {
            "loaders dirty"
        } else {
            "labels only"
        };
        println!("    {:<40} {rule:<12} {loads}", r.repo);
        for p in r.paths.iter().take(3) {
            println!("        via {p}");
        }
    }
    if o.repos.len() > max {
        println!("    ... {} more", o.repos.len() - max);
    }
    println!("  packages dirtied: {}", o.dirty_packages);
    for (rule, n) in o.packages_by_rule() {
        println!("    {:<24} {n}", rule_name(rule));
    }
    for p in o.packages.iter().take(max) {
        println!("    {:<60} {}", p.package, rule_name(p.rule));
    }
    if o.packages.len() > max {
        println!("    ... {} more", o.packages.len() - max);
    }
    println!(
        "  limit:           {} of limits.max_dirty_packages = {} -> {}",
        o.dirty_packages,
        o.max_dirty_packages,
        if o.cold_rebuild {
            "COLD REBUILD"
        } else {
            "splice"
        }
    );
    println!("  external direct targets: {}", o.external_direct_targets);
    println!(
        "  affected (lower bound): {}   [the reverse walk from what the base index already \
         knows changed; the run's number needs a bazel query at head]",
        o.affected_lower_bound
    );
    if o.cold_rebuild {
        println!(
            "  note: past the limit the engine re-queries //... — this replay says the fallback \
             fires, not what it would then answer."
        );
    }
    if let Some(r) = recorded {
        println!(
            "  recorded run:    global={} dirty_packages={} cold_rebuild={} affected={}",
            r.global,
            r.dirty_packages,
            r.cold_rebuild,
            r.affected
                .map(|a| a.to_string())
                .unwrap_or_else(|| "-".into())
        );
        if o.base_kind.exact_at_base() {
            println!(
                "  note: this replayed against the base index the run itself used, so a \
                 difference here is the engine rather than graph drift."
            );
        } else {
            println!(
                "  note: the stored index is at the pin, not at {}, so a difference here can be \
                 graph drift rather than an engine change. --baseline compares replay to replay.",
                r.base
            );
        }
    }
}

fn rule_name(r: PackageRule) -> &'static str {
    match r {
        PackageRule::ChangedFile => "changed file",
        PackageRule::BzlClosure => "bzl closure",
        PackageRule::ExternalBzlClosure => "external via bzl",
        PackageRule::ExternalPkgExt => "external via pkg_ext",
    }
}

fn render_all(report: &ReplayReport) {
    let run: &ReplayRun = &report.run;
    let globals = run.outcomes.iter().filter(|o| o.global).count();
    let colds = run.outcomes.iter().filter(|o| o.cold_rebuild).count();
    let indet = run
        .outcomes
        .iter()
        .filter(|o| !o.indeterminate.is_empty())
        .count();
    println!(
        "{}: replayed {} change sets against the index at {}",
        run.subject,
        run.outcomes.len(),
        run.pin
    );
    println!("  global: {globals}   cold rebuild: {colds}   indeterminate (not gated): {indet}");
    let own = report.archived_bases();
    if own > 0 {
        println!(
            "  base index: {own} of {} replayed against the base index the run itself spliced \
             onto (bases/<sha>.bin); the rest against the index at the pin",
            run.outcomes.len()
        );
    }
    let rederived = run.outcomes.iter().filter(|o| o.rederived).count();
    if rederived > 0 {
        let costs: Vec<u64> = run.outcomes.iter().filter_map(|o| o.rederive_ms).collect();
        let total: u64 = costs.iter().sum();
        let worst = costs.iter().copied().max().unwrap_or(0);
        println!(
            "  REDERIVED: {rederived} of {} replays used externals tables rebuilt from the \
             repository text at their own base, not the tables the run stored \
             ({} rebuilds, {total} ms total, {worst} ms worst)",
            run.outcomes.len(),
            costs.len(),
        );
    }
    for s in &report.skipped {
        println!("  SKIPPED {s}");
    }
    for d in &report.divergences {
        println!("  DIVERGENCE {d}");
    }
    let against = report.compared_against;
    if report.divergences.is_empty() {
        println!("  no divergence from {against}");
    } else {
        println!("  {} divergences from {against}", report.divergences.len());
    }
}
