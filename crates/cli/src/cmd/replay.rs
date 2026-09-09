//! `cambi bench replay` — the engine's static stages, offline, against a
//! published run's own artefacts.
//!
//! It lives under `bench` because everything it reads is a benchmark artefact:
//! the index at the pin, the recorded change sets and the recorded answers.
//! It runs no Bazel and needs no checkout, so it is the check to run *before*
//! the benchmark machine is started, not after.
//!
//! See [`cambium_bench::replay`] for what a replay can and cannot know; the
//! rendering here repeats the important half in the output, because a number
//! whose limits are only in a doc comment will be quoted without them.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use cambium_bench::checkout::SubjectsDir;
use cambium_bench::compare::ChangesResult;
use cambium_bench::history::HistoryResult;
use cambium_bench::manifest::Manifest;
use cambium_bench::replay::{
    self, PackageRule, PathsCache, RecordedAnswer, ReplayOutcome, ReplayRun,
};
use cambium_index::IndexFile;
use clap::Args as ClapArgs;

use crate::cmd::Context;
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

    /// The index to replay against. Default:
    /// <results-dir>/<subject>/index-<pin>.bin.
    #[arg(long)]
    pub index: Option<PathBuf>,
    /// Where the recorded results are.
    #[arg(long, default_value = "bench-results")]
    pub results_dir: PathBuf,
    /// Path to subject_repos.toml (default: discovered upward from the cwd).
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    /// Mirrors and caches, for the generated per-subject configuration
    /// (default: $CAMBIUM_SUBJECTS_DIR, else ~/.cache/cambium/subjects).
    #[arg(long)]
    pub subjects_dir: Option<PathBuf>,
    /// Bazel version suffix on the result and index file names.
    #[arg(long)]
    pub bazel_version: Option<String>,

    /// A cached diff per change set, written by tools/bench/precheck.sh. The
    /// recorded change sets carry `(base, head)` and no paths, so without
    /// either this or --repo there is nothing to replay.
    #[arg(long = "paths-file")]
    pub paths_file: Option<PathBuf>,
    /// A git repository (a clone or the subject's mirror) to take the diff
    /// from, and to read MODULE.bazel at either revision. With it a replay
    /// decides the change sets it otherwise marks indeterminate.
    #[arg(long = "repo")]
    pub repo: Option<PathBuf>,

    /// Rebuild the externals side tables from the repository text at each
    /// change set's base before replaying it, instead of reading back the
    /// tables the run's cambium derived. This is what makes an index-time
    /// change — `build_repo_inputs` and what feeds it — visible offline.
    /// Requires --repo. A re-derived replay is not comparable against a plain
    /// one as engine evidence; --baseline says which side was which.
    #[arg(long)]
    pub rederive: bool,

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
    #[arg(long, value_enum, default_value_t = DataFormat::Text)]
    pub output: DataFormat,
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    let (_, manifest) = match &args.manifest {
        Some(p) => (p.clone(), Manifest::load(p)?),
        None => Manifest::discover(&ctx.root)?,
    };
    let entry = manifest.get(&args.subject)?.clone();
    let subjects = match &args.subjects_dir {
        Some(p) => SubjectsDir::new(p.clone()),
        None => SubjectsDir::from_env(),
    };
    let version = args
        .bazel_version
        .clone()
        .or_else(|| entry.bazel_version.clone())
        .filter(|v| !v.is_empty());
    // Generated, not read from a worktree, so this costs no Bazel and no
    // checkout — the same configuration `bench changes` would have run under.
    let cfg = subjects.config_for(&entry, version.as_deref());

    let results = args.results_dir.join(&entry.id);
    let index_path = match &args.index {
        Some(p) => p.clone(),
        None => results.join(cambium_bench::util::versioned_name(
            &format!("index-{}", entry.pin),
            version.as_deref(),
            "bin",
        )),
    };
    anyhow::ensure!(
        index_path.exists(),
        "no stored index at {}; pull one from the benchmark machine \
         (tools/bench/precheck.sh) or pass --index",
        index_path.display()
    );
    let file = IndexFile::open(&index_path)
        .with_context(|| format!("opening {}", index_path.display()))?;
    let view = file.view();

    anyhow::ensure!(
        !args.rederive || args.repo.is_some(),
        "--rederive reads MODULE.bazel, WORKSPACE and the workspace .bzl closure at each base \
         revision, so it needs --repo <git dir>"
    );
    let repo = args
        .repo
        .as_ref()
        .map(|p| cambium_diff::Repo::new(p.clone()));
    let paths_cache = match &args.paths_file {
        Some(p) => Some(PathsCache::read(p)?),
        None => {
            let p = default_paths_file(&results, &entry.pin, version.as_deref());
            p.exists().then(|| PathsCache::read(&p)).transpose()?
        }
    };

    // What to replay.
    let recorded = load_recorded(&results, &entry.pin, version.as_deref(), args.history)?;
    let wanted: Vec<RecordedAnswer> = if !args.paths.is_empty() {
        vec![RecordedAnswer {
            base: "explicit".into(),
            head: "paths".into(),
            ..Default::default()
        }]
    } else if let Some(cs) = &args.change_set {
        let (b, h) = cs
            .split_once("..")
            .context("--change-set takes base..head")?;
        vec![find(&recorded, b, h)
            .with_context(|| format!("no recorded change set {cs}"))?
            .clone()]
    } else if let Some(sha) = &args.commit {
        vec![recorded
            .iter()
            .find(|r| r.head.starts_with(sha.as_str()) || sha.starts_with(r.head.as_str()))
            .with_context(|| format!("no recorded change set or commit at {sha}"))?
            .clone()]
    } else if args.all {
        recorded.clone()
    } else {
        anyhow::bail!(
            "say what to replay: --paths, --change-set base..head, --commit sha, or --all"
        )
    };

    let explicit = !args.paths.is_empty();
    let mut rederived = replay::RederiveCache::new();
    let mut outcomes: Vec<ReplayOutcome> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for want in &wanted {
        let changes = if explicit {
            replay::parse_name_status(&args.paths)
        } else if let Some(r) = &repo {
            r.diff(&want.base, &want.head)
                .with_context(|| format!("git diff {}..{}", want.base, want.head))?
        } else if let Some(c) = &paths_cache {
            match c.lookup(&want.base, &want.head) {
                Some(c) => c,
                None => {
                    skipped.push(format!("{}: not in the paths cache", want.key()));
                    continue;
                }
            }
        } else {
            anyhow::bail!(
                "the recorded change sets carry no diff; pass --repo <git dir> or --paths-file \
                 (tools/bench/precheck.sh writes one)"
            );
        };
        let texts: Box<dyn cambium_engine::externals::WorkspaceText> = match &repo {
            Some(r) => Box::new(GitTextOwned(r.clone())),
            None => Box::new(replay::NoText),
        };
        // With --rederive the index the stages see is the stored one with its
        // externals tables rebuilt from the base's own text; without it, the
        // stored index exactly as the run left it.
        let mut cost = None;
        let replay_view = if args.rederive {
            match rederived.ensure(view, &want.base, texts.as_ref()) {
                Ok(ms) => {
                    cost = ms;
                    rederived.get(&want.base).unwrap_or(view)
                }
                Err(e) => {
                    skipped.push(format!("{}: cannot re-derive: {e:#}", want.key()));
                    continue;
                }
            }
        } else {
            view
        };
        let mut o = replay::replay(
            replay_view,
            &cfg,
            &want.base,
            &want.head,
            &changes,
            texts.as_ref(),
        );
        o.rederived = args.rederive;
        o.rederive_ms = cost;
        outcomes.push(o);
    }

    let run = ReplayRun {
        schema: replay::REPLAY_SCHEMA,
        subject: entry.id.clone(),
        pin: entry.pin.clone(),
        outcomes,
    };
    if let Some(p) = &args.save_baseline {
        run.write(p)?;
    }

    // Divergences: against a previous replay when one is given (both sides on
    // the same index, so every difference is the engine's), otherwise against
    // what the run recorded.
    let (divergences, against) = match &args.baseline {
        Some(p) => (run.divergences_against(&ReplayRun::read(p)?), "baseline"),
        None if explicit => (Vec::new(), "nothing"),
        None => {
            let mut d = Vec::new();
            for o in &run.outcomes {
                if let Some(r) = find(&recorded, &o.base, &o.head) {
                    d.extend(replay::diverge(r, o));
                }
            }
            (d, "the recorded run")
        }
    };

    match args.output {
        DataFormat::Json => {
            let doc = serde_json::json!({
                "schema": replay::REPLAY_SCHEMA,
                "subject": run.subject,
                "pin": run.pin,
                "index": index_path.display().to_string(),
                "compared_against": against,
                "rederived": args.rederive,
                "replays": run.outcomes,
                "divergences": divergences,
                "skipped": skipped,
            });
            println!("{}", serde_json::to_string_pretty(&doc)?);
        }
        DataFormat::Text => {
            if args.all || run.outcomes.len() > 1 {
                render_all(&run, &divergences, &skipped, against);
            } else {
                for o in &run.outcomes {
                    render_one(o, find(&recorded, &o.base, &o.head), args.max_listed);
                }
                for d in &divergences {
                    println!("DIVERGENCE {d}");
                }
            }
        }
    }

    Ok(if divergences.is_empty() {
        crate::EXIT_OK
    } else {
        crate::EXIT_ERROR
    })
}

/// A [`cambium_diff::Repo`] that owns itself, so it can outlive a borrow.
struct GitTextOwned(cambium_diff::Repo);

impl cambium_engine::externals::WorkspaceText for GitTextOwned {
    fn text_at(&self, rev: &str, path: &str) -> Option<String> {
        self.0
            .read_at(rev, path)
            .and_then(|b| String::from_utf8(b).ok())
    }
}

fn find<'a>(recorded: &'a [RecordedAnswer], base: &str, head: &str) -> Option<&'a RecordedAnswer> {
    recorded
        .iter()
        .find(|r| pre(&r.base, base) && pre(&r.head, head))
}

fn pre(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
}

fn default_paths_file(results: &Path, pin: &str, version: Option<&str>) -> PathBuf {
    results.join(cambium_bench::util::versioned_name(
        &format!("paths-{pin}"),
        version,
        "json",
    ))
}

/// Every recorded answer under one subject: the change sets always, the
/// history commits when asked.
fn load_recorded(
    results: &Path,
    pin: &str,
    version: Option<&str>,
    history: bool,
) -> Result<Vec<RecordedAnswer>> {
    let mut out = Vec::new();
    let changes = results
        .join("changes")
        .join(cambium_bench::util::versioned_name(
            &format!("changes-{pin}"),
            version,
            "json",
        ));
    if changes.exists() {
        out.extend(replay::recorded_from_changes(&ChangesResult::read(
            &changes,
        )?));
    }
    if history {
        let h = results
            .join("history")
            .join(cambium_bench::util::versioned_name(
                &format!("history-{pin}"),
                version,
                "json",
            ));
        if h.exists() {
            let s = std::fs::read_to_string(&h)?;
            let r: HistoryResult =
                serde_json::from_str(&s).with_context(|| format!("parsing {}", h.display()))?;
            out.extend(replay::recorded_from_history(&r));
        }
    }
    anyhow::ensure!(
        !out.is_empty(),
        "no recorded results under {}: nothing to replay against",
        results.display()
    );
    Ok(out)
}

fn render_one(o: &ReplayOutcome, recorded: Option<&RecordedAnswer>, max: usize) {
    println!("{} .. {}", o.base, o.head);
    println!("  changed files:   {}", o.changed_files);
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
        println!(
            "  note: the stored index is at the pin, not at {}, so a difference here can be \
             graph drift rather than an engine change. --baseline compares replay to replay.",
            r.base
        );
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

fn render_all(
    run: &ReplayRun,
    divergences: &[replay::Divergence],
    skipped: &[String],
    against: &str,
) {
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
    for s in skipped {
        println!("  SKIPPED {s}");
    }
    for d in divergences {
        println!("  DIVERGENCE {d}");
    }
    if divergences.is_empty() {
        println!("  no divergence from {against}");
    } else {
        println!("  {} divergences from {against}", divergences.len());
    }
}
