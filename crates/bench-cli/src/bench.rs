//! `cambium-bench` — the benchmark (docs/METHODOLOGY.md).
//!
//! The command tree is the vocabulary: `subjects` for what a run points at,
//! `history` and `changes` for the two workloads, `stages` and `sweep` for the
//! engine micro-benchmarks underneath them, `regress` to gate one result file
//! against another, `report` to regenerate `docs/RESULTS.md`, and `run` for
//! the whole published sequence.
//!
//! Every subcommand takes a `<subject>` — a name from `subject_repos.toml`, or
//! `synth:<preset>` — and every one of them is idempotent and resumable: what
//! finished is recorded in `bench-results/<subject>/state.json`, and rerunning
//! picks up from there rather than redoing an hour of `bazel query`. Progress
//! is logged at `info`, so a long run is followable with `--log info`.
//!
//! This file marshals arguments and prints answers. What a verb *does* is
//! [`cambium_bench::run`], against a [`cambium_bench::session::Session`].

use std::path::PathBuf;

use anyhow::Result;
use cambium_bench::plan::{Plan, Size, StepKind};
use cambium_bench::run as verbs;
use cambium_bench::run::{CompareRequest, HistoryRequest, Labels, LabelsQuery, Selection};
use clap::{Args as ClapArgs, Subcommand};

use crate::Context;
use crate::DataFormat;

/// Flags every `bench` subcommand shares.
#[derive(ClapArgs, Clone, Default)]
pub struct Common {
    /// Path to subject_repos.toml (default: discovered upward from the cwd).
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    /// Where results go: <results-dir>/<subject>/<workload>/.
    #[arg(long, default_value = "bench-results")]
    pub results_dir: PathBuf,
    /// Mirrors, worktrees, emitted synthetic subjects and caches (default:
    /// ~/.cache/cambium/subjects).
    #[arg(long, env = cambium_bench::checkout::SUBJECTS_DIR_ENV)]
    pub subjects_dir: Option<PathBuf>,
    /// Bazel version for bazelisk (USE_BAZEL_VERSION); also suffixes the
    /// result file names. Default: whatever the subject's .bazelversion says.
    #[arg(long)]
    pub bazel_version: Option<String>,
    /// Seed for the deterministic change-set sampling.
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
    /// Laptop-sized: 50 commits, 50 change sets, Tango on the first 10.
    ///
    /// Sugar for those three flags and nothing else. It is not a mode: the
    /// results land in the same tree as any other run, and the sizes that
    /// produced them are inside the result file, so a short run and a
    /// published one differ only in size.
    #[arg(long)]
    pub quick: bool,
}

#[derive(Subcommand)]
pub enum Args {
    /// List the subjects, or fetch and cold-build one.
    Subjects {
        #[command(subcommand)]
        what: SubjectArgs,
    },
    /// The history workload: the last N first-parent commits, applied one at a
    /// time to an index built at the start of the window.
    ///
    /// cambium-only: the other tools keep no state between runs, so there is
    /// no state to carry. It yields the per-commit cost, the global rate, the
    /// cold-rebuild rate, and the rebuild oracle.
    History {
        /// A name from subject_repos.toml, or synth:<preset>.
        subject: String,
        /// First-parent commits before the pin. Defaults to the subject's
        /// history window.
        #[arg(long)]
        commits: Option<usize>,
        /// `rebuild`, or `none` to skip it. Only ever right when the cold
        /// build is what will not finish.
        #[arg(long = "oracle", value_delimiter = ',', default_value = "rebuild")]
        oracles: Vec<String>,
        /// Start over rather than resuming the recorded history.
        #[arg(long)]
        restart: bool,
        #[command(flatten)]
        common: Common,
    },
    /// The change-set workload: every tool on each `(base, head)` change set,
    /// scored by the oracles and the disagreement taxonomy.
    Changes {
        /// A name from subject_repos.toml, or synth:<preset>.
        subject: String,
        /// How many change sets to compare.
        #[arg(long = "change-sets")]
        change_sets: Option<usize>,
        /// pr, adjacent, or stress. Comma-separated; default: all three.
        #[arg(long = "kind", value_delimiter = ',')]
        kinds: Vec<String>,
        /// cambium, bazel-diff, tango. Comma-separated; default: all three.
        /// cambium always runs — it is what the others are being compared
        /// against — so naming it alone measures cambium with no foils.
        #[arg(long = "tools", value_delimiter = ',')]
        tools: Vec<String>,
        /// rdeps, load, or none. Comma-separated; default: both.
        #[arg(long = "oracle", value_delimiter = ',', default_value = "rdeps,load")]
        oracles: Vec<String>,
        /// Cap on the source labels the rdeps oracle queries per change set.
        /// 0, the default, is the full oracle: every changed file. A positive
        /// value is an escape hatch for a subject whose change sets are too
        /// large to query whole, and weakens the check.
        #[arg(long, default_value_t = 0)]
        oracle_sample: usize,
        /// A change-set JSON to run, instead of selecting one.
        #[arg(long = "change-set-file")]
        change_set_file: Option<PathBuf>,
        /// Stress change sets per category when selecting.
        #[arg(long, default_value_t = 20)]
        stress_cap: usize,
        /// Print the selected change sets and stop.
        #[arg(long)]
        select_only: bool,
        /// Compare every change set again rather than resuming.
        #[arg(long)]
        restart: bool,
        #[command(flatten)]
        common: Common,
    },
    /// The whole run, as `tools/bench/run-published.sh` runs it: per subject
    /// the fetch, the cold build, the two workloads and the report, then one
    /// report over every subject. Each step is a child `cambium-bench`.
    ///
    /// A failed step never stops the run; a failed fetch or build skips the
    /// rest of that subject. The launcher (no verb, on a terminal) composes
    /// the same plan on a screen.
    Run(RunArgs),
    /// Engine stage micro-benchmarks, over a synthetic subject.
    Stages {
        /// synth:<preset>. A real subject has no generated graph to time
        /// stages over.
        subject: String,
        /// Stages to run; defaults to every in-memory stage.
        #[arg(long = "stage", value_delimiter = ',')]
        stages: Vec<String>,
        #[arg(long, default_value_t = 5)]
        runs: usize,
        /// Where to write the JSON. Defaults to
        /// bench-results/<subject>/stages/<ts>.json.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Also write a markdown summary table here.
        #[arg(long)]
        markdown: Option<PathBuf>,
        /// Print the results instead of writing a file.
        #[arg(long)]
        stdout: bool,
        #[command(flatten)]
        common: Common,
    },
    /// Vary one parameter and report how a stage scales.
    Sweep {
        /// synth:<preset>: the graph the sweep varies from.
        subject: String,
        #[arg(long)]
        stage: String,
        #[arg(long, default_value = "targets")]
        param: String,
        #[arg(long, value_delimiter = ',')]
        values: Vec<String>,
        #[arg(long, default_value_t = 3)]
        runs: usize,
        #[arg(long)]
        out: Option<PathBuf>,
        /// Write an SVG of ms against the parameter, on log-log axes.
        #[arg(long)]
        plot: Option<PathBuf>,
        #[command(flatten)]
        common: Common,
    },
    /// Compare two stage result files and fail on a regression.
    Regress {
        /// The baseline result file.
        baseline: PathBuf,
        /// The result file to gate.
        current: PathBuf,
        /// Percentage a stage may slow down before it counts as a regression.
        #[arg(long, default_value_t = 15.0)]
        threshold: f64,
    },
    /// Replay the engine's static stages offline, against a stored index and
    /// a published run's own answers.
    ///
    /// No Bazel, no checkout, milliseconds a change set: the check to run
    /// before the benchmark machine is started. See
    /// `docs/METHODOLOGY.md`, "Validating an engine change before a run".
    Replay(Box<crate::replay::Args>),
    /// Pull a subject's results off the benchmark box for an offline replay:
    /// the index at the pin, the recorded results, the archived base and head
    /// indexes, and the diff of every recorded change set.
    ///
    /// Read-only on the box. A subject already under --cache is not pulled
    /// again; delete its directory to refresh it.
    PullResults(PullArgs),
    /// Print the label sets a change-set comparison stored.
    ///
    /// `changes-<pin>.json` keeps per-class counts and at most a handful of
    /// example labels; the sets themselves are written whole beside it. This
    /// reads them back — offline, no Bazel, no checkout.
    Labels {
        /// A name from subject_repos.toml, or synth:<preset>.
        subject: String,
        /// Which change set, as base..head. Abbreviated revisions are fine.
        #[arg(long = "change-set")]
        change_set: String,
        /// One stored set (a tool's name, `cambium` or `oracle`), or the tool
        /// a disagreement was with when --class or --side narrows one.
        #[arg(long)]
        tool: Option<String>,
        /// Only disagreements filed in this class.
        #[arg(long = "class")]
        class: Option<String>,
        /// Only disagreements on this side: tool or cambium.
        #[arg(long)]
        side: Option<String>,
        /// Print how each tool's set stands against cambium's instead of the
        /// labels: only-tool, only-cambium, both.
        #[arg(long)]
        diff: bool,
        #[command(flatten)]
        common: Common,
    },
    /// Generated Bazel graphs and workspaces.
    #[command(subcommand)]
    Synth(crate::synth::Args),
    /// Follow a run: one row per subject, polled off bench-results/.
    ///
    /// A pure reader. It holds nothing open on the run, so it works on a run
    /// in progress, on a finished one, and on results copied off the
    /// benchmark box. `--once` prints the same table and exits.
    Watch(crate::watch::Args),
    /// Regenerate docs/RESULTS.md from bench-results/.
    Report {
        /// Rewrite only this subject's section. Every subject that has results
        /// is rendered either way — the top table is built from all of them —
        /// but naming one makes a per-subject nightly job's intent explicit.
        subject: Option<String>,
        /// The document to update.
        #[arg(long)]
        doc: Option<PathBuf>,
        #[command(flatten)]
        common: Common,
    },
}

#[derive(Subcommand)]
pub enum SubjectArgs {
    /// Print every subject in the manifest, plus the synthetic presets.
    List {
        #[command(flatten)]
        common: Common,
    },
    /// Mirror the subject and check out its pin.
    Fetch {
        /// A name from subject_repos.toml, or synth:<preset>. Omit for every
        /// subject the manifest does not skip.
        subject: Option<String>,
        #[command(flatten)]
        common: Common,
    },
    /// Cold-build an index at the pin and publish it to the subject's store.
    Build {
        /// A name from subject_repos.toml, or synth:<preset>. Omit for every
        /// subject the manifest does not skip.
        subject: Option<String>,
        /// Revision to build. Defaults to the manifest pin.
        #[arg(long)]
        at: Option<String>,
        /// Rebuild even if this revision was already built.
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        common: Common,
    },
}

/// `cambium-bench run`: the published sequence, from flags.
#[derive(ClapArgs, Clone, Debug)]
pub struct RunArgs {
    /// Subjects to run, cheapest first. Default: every subject the manifest
    /// does not skip.
    #[arg(long = "subject", value_name = "S", value_delimiter = ',')]
    pub subjects: Vec<String>,
    /// Which steps to take: fetch, build, history, changes, report.
    /// Comma-separated; default: all of them.
    #[arg(long = "steps", value_delimiter = ',')]
    pub steps: Vec<String>,
    /// How big: smoke, laptop, or published.
    #[arg(long, default_value = "published")]
    pub size: String,
    /// The fifteen-minute pass before the eight-hour one: `--size smoke`.
    #[arg(long, conflicts_with = "size")]
    pub smoke: bool,
    /// The foils `changes` compares against. Default: bazel-diff and tango.
    #[arg(long = "tools", value_delimiter = ',')]
    pub tools: Vec<String>,
    /// `--restart` on `history`: redo the walk rather than resume it.
    #[arg(long)]
    pub restart_history: bool,
    /// `--restart` on `changes`: compare every change set again, which a
    /// fix to a tool driver needs (a resumed change set keeps the old
    /// tool's answer).
    #[arg(long)]
    pub restart_changes: bool,
    /// Where results go. Default: bench-results/, or bench-results-smoke/
    /// for a smoke run.
    #[arg(long)]
    pub results_dir: Option<PathBuf>,
    /// Refuse to start unless both `cambi` and this binary were built from
    /// this git sha: a run on a stale binary is hours lost.
    #[arg(long, value_name = "SHA")]
    pub require_sha: Option<String>,
    /// Print the steps and run nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Path to subject_repos.toml (default: discovered upward from the cwd).
    #[arg(long)]
    pub manifest: Option<PathBuf>,
}

/// `cambium-bench pull-results`.
#[derive(ClapArgs, Clone, Debug)]
pub struct PullArgs {
    /// Subjects to pull. Default: every subject the manifest does not skip.
    pub subjects: Vec<String>,
    /// Where the pulled trees go: <cache>/<subject>/.
    #[arg(long, default_value = "bench-precheck")]
    pub cache: PathBuf,
    /// The box's address, instead of asking EC2 for the running instance.
    #[arg(long)]
    pub host: Option<String>,
    /// The ssh user on the box.
    #[arg(long, default_value = "ubuntu")]
    pub user: String,
    /// The ssh key.
    #[arg(long, value_name = "PEM")]
    pub key: Option<PathBuf>,
    /// The EC2 region the box is in.
    #[arg(long, default_value = "us-east-2", env = "AWS_REGION")]
    pub region: String,
    /// The cambium checkout on the box, relative to the user's home.
    #[arg(long, default_value = "cambium")]
    pub box_repo: String,
    /// The mirrors directory on the box, relative to the user's home.
    #[arg(long, default_value = ".cache/cambium/corpus")]
    pub box_subjects: String,
    /// Do not derive the change-set diffs on the box.
    #[arg(long)]
    pub no_paths: bool,
    /// Path to subject_repos.toml (default: discovered upward from the cwd).
    #[arg(long)]
    pub manifest: Option<PathBuf>,
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    match args {
        Args::Subjects { what } => do_subjects(ctx, what),
        Args::Synth(args) => crate::synth::run(ctx, args),
        Args::Replay(args) => crate::replay::run(ctx, *args),
        Args::Watch(args) => crate::watch::run(ctx, args),
        Args::Run(args) => do_run(ctx, &args),
        Args::PullResults(args) => do_pull(ctx, &args),
        Args::History {
            subject,
            commits,
            oracles,
            restart,
            common,
        } => {
            let session = ctx.session(&common)?;
            let run = session.run_subject(&subject)?;
            let (path, result) = verbs::run_history(
                &run,
                &HistoryRequest {
                    commits,
                    oracles,
                    resume: !restart,
                },
            )?;
            println!("{}", path.display());
            if let Some(o) = &result.oracle {
                if !o.ok {
                    anyhow::bail!("history oracle failed: {}", o.diff.summary());
                }
            }
            Ok(crate::EXIT_OK)
        }
        Args::Changes {
            subject,
            change_sets,
            kinds,
            tools,
            oracles,
            oracle_sample,
            change_set_file,
            stress_cap,
            select_only,
            restart,
            common,
        } => {
            let session = ctx.session(&common)?;
            let run = session.run_subject(&subject)?;
            let selection = Selection {
                kinds,
                cap: verbs::change_set_cap(&session, change_sets),
                stress_cap,
            };
            if select_only {
                let (_, set) = verbs::select_change_sets(&run, &selection)?;
                print_change_sets(&set, ctx.format.data())?;
                return Ok(crate::EXIT_OK);
            }
            let compared = verbs::compare(
                &run,
                &CompareRequest {
                    change_set_file,
                    selection,
                    oracle_sample,
                    oracles,
                    tools,
                    restart,
                },
                &ctx.tools,
            )?;
            println!("{}", verbs::compare_summary(&compared.result));
            println!("{}", compared.path.display());
            Ok(if compared.result.summary.pass {
                crate::EXIT_OK
            } else {
                crate::EXIT_ERROR
            })
        }
        Args::Labels {
            subject,
            change_set,
            tool,
            class,
            side,
            diff,
            common,
        } => {
            let session = ctx.session(&common)?;
            let run = session.subject(&subject)?;
            let answer = verbs::read_labels(
                &run,
                &LabelsQuery {
                    change_set,
                    tool,
                    class,
                    side,
                    diff,
                },
            )?;
            print_labels(&answer);
            Ok(crate::EXIT_OK)
        }
        Args::Stages {
            subject,
            stages,
            runs,
            out,
            markdown,
            stdout,
            common,
        } => {
            let session = ctx.session(&common)?;
            let preset = verbs::preset_of(&subject, "stages")?;
            let dir = verbs::stage_dir(&session, &subject, "stages")?;
            let opts = cambium_bench::runner::RunOptions {
                preset,
                stages,
                runs,
                out: if stdout {
                    None
                } else {
                    Some(out.unwrap_or_else(|| dir.join(timestamped())))
                },
                markdown,
            };
            cambium_bench::runner::run(&opts)?;
            Ok(crate::EXIT_OK)
        }
        Args::Sweep {
            subject,
            stage,
            param,
            values,
            runs,
            out,
            plot,
            common,
        } => {
            let session = ctx.session(&common)?;
            let preset = verbs::preset_of(&subject, "sweep")?;
            let dir = verbs::stage_dir(&session, &subject, "sweep")?;
            let values = values
                .iter()
                .map(|v| cambium_bench::util::parse_size(v))
                .collect::<Result<Vec<u64>>>()?;
            anyhow::ensure!(!values.is_empty(), "--values is required");
            let plot = plot.or_else(|| {
                (stage == cambium_bench::legacy::STAGE).then(|| dir.join("bench-legacy.svg"))
            });
            cambium_bench::runner::sweep(&cambium_bench::runner::SweepOptions {
                stage,
                param,
                values,
                runs,
                out: Some(out.unwrap_or_else(|| dir.join(timestamped()))),
                plot,
                base_preset: preset,
            })?;
            Ok(crate::EXIT_OK)
        }
        Args::Regress {
            baseline,
            current,
            threshold,
        } => {
            let regressions = cambium_bench::runner::compare_files(&baseline, &current, threshold)?;
            Ok(if regressions == 0 {
                crate::EXIT_OK
            } else {
                eprintln!(
                    "cambium-bench regress: {regressions} stage(s) regressed by more than {threshold}%"
                );
                crate::EXIT_ERROR
            })
        }
        Args::Report {
            subject,
            doc,
            common,
        } => {
            let session = ctx.session(&common)?;
            let (doc, _) = verbs::regenerate_report(&session, subject.as_deref(), doc.as_deref())?;
            println!("{}", doc.display());
            Ok(crate::EXIT_OK)
        }
    }
}

fn timestamped() -> String {
    format!("{}.json", cambium_bench::util::now_unix())
}

fn do_subjects(ctx: &Context, what: SubjectArgs) -> Result<u8> {
    match what {
        SubjectArgs::List { common: c } => {
            let session = ctx.session(&c)?;
            println!("real subjects ({}):", session.manifest_path.display());
            for e in &session.manifest.repos {
                println!(
                    "  {:<14} {:<12} window {:<4}{}  {}",
                    e.id,
                    &e.pin[..12],
                    e.history_window,
                    if e.skip { "  (skip)" } else { "" },
                    e.notes
                );
            }
            println!("\nsynthetic subjects (synth:<preset>):");
            for p in cambium_synth::GraphSpec::PRESETS {
                println!("  synth:{p}");
            }
            Ok(crate::EXIT_OK)
        }
        SubjectArgs::Fetch { subject, common: c } => {
            let session = ctx.session(&c)?;
            for name in session.subject_names(subject) {
                let run = session.run_subject(&name)?;
                println!("{}", verbs::fetch(&run)?.display());
            }
            Ok(crate::EXIT_OK)
        }
        SubjectArgs::Build {
            subject,
            at,
            force,
            common: c,
        } => {
            let session = ctx.session(&c)?;
            for name in session.subject_names(subject) {
                let run = session.run_subject(&name)?;
                let built = verbs::build(&run, at.as_deref(), force)?;
                if let Some(w) = &built.warning {
                    eprintln!("cambium-bench: WARNING: {w}");
                }
                println!("{}", built.index.display());
            }
            Ok(crate::EXIT_OK)
        }
    }
}

fn print_change_sets(
    set: &cambium_bench::changes::ChangeSetSet,
    output: Option<DataFormat>,
) -> Result<()> {
    match output {
        None => {}
        Some(DataFormat::Json) => println!("{}", serde_json::to_string_pretty(set)?),
        Some(DataFormat::Jsonl) => println!("{}", crate::result_event(set)?),
        Some(DataFormat::Text) => {
            for p in &set.change_sets {
                println!(
                    "{}\t{}\t{}\t{}",
                    p.kind.name(),
                    p.category.map_or("-", |c| c.name()),
                    p.base,
                    p.head
                );
            }
        }
    }
    Ok(())
}

fn print_labels(answer: &Labels) {
    match answer {
        Labels::Diff(diffs) => {
            for d in diffs {
                println!(
                    "{}\tonly-tool {}\tonly-cambium {}\tboth {}",
                    d.tool, d.only_tool, d.only_cambium, d.both
                );
            }
        }
        Labels::Classes(rows) => {
            for row in rows {
                println!(
                    "{}\t{}\t{}\t{}",
                    row.tool,
                    cambium_bench::labels::side_name(row.side),
                    row.class.name(),
                    row.label
                );
            }
        }
        Labels::Sets(sets) => {
            // Named only when there is more than one of them: a `--tool`
            // answer is meant to be pipeable into `comm` or `grep -f`.
            let header = sets.len() > 1;
            for (name, labels) in sets {
                if header {
                    println!("# {name}");
                }
                for label in labels {
                    println!("{label}");
                }
            }
        }
    }
}

/// The plan `run` was asked for.
pub fn plan_of(ctx: &Context, args: &RunArgs) -> Result<Plan> {
    let session = ctx.session(&Common {
        manifest: args.manifest.clone(),
        ..Default::default()
    })?;
    let subjects = match args.subjects.is_empty() {
        true => session.subject_names(None),
        false => args.subjects.clone(),
    };
    let size = if args.smoke {
        Size::Smoke
    } else {
        Size::parse(&args.size)
            .ok_or_else(|| anyhow::anyhow!("--size is smoke, laptop or published"))?
    };
    let steps = match args.steps.is_empty() {
        true => StepKind::ALL.to_vec(),
        false => args
            .steps
            .iter()
            .map(|s| {
                StepKind::parse(s.trim()).ok_or_else(|| {
                    anyhow::anyhow!("unknown step {s:?}: fetch, build, history, changes, report")
                })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    let mut plan = Plan::published(subjects);
    plan.size = size;
    plan.steps = steps;
    plan.restart_history = args.restart_history;
    plan.restart_changes = args.restart_changes;
    plan.results_dir = args.results_dir.clone();
    if !args.tools.is_empty() {
        plan.tools = args.tools.clone();
    }
    Ok(plan)
}

fn do_run(ctx: &Context, args: &RunArgs) -> Result<u8> {
    let plan = plan_of(ctx, args)?;
    if args.dry_run {
        for step in plan.steps() {
            println!("{}", step.command_line());
        }
        return Ok(crate::EXIT_OK);
    }
    // The preflight the driver used to do by hand: a run on a stale binary
    // is hours lost, so both binaries have to be built from the tree.
    if let Some(sha) = &args.require_sha {
        let session = ctx.session(&Common::default())?;
        cambium_bench::plan::require_sha(session.cambi()?.version(), sha)?;
        println!(
            "=== {} tree {sha} ===\n=== harness {} ===",
            session.cambi()?.version(),
            crate::VERSION
        );
    }
    if plan.size == Size::Smoke {
        println!(
            "=== SMOKE: results in {}/, 3 history commits, 1 change set per subject ===",
            plan.results().display()
        );
    }
    let failures = cambium_bench::plan::run_plan(&plan, &std::env::current_exe()?)?;
    println!(
        "{}",
        match (plan.size, failures) {
            (Size::Smoke, 0) => "SMOKE RUN DONE".to_string(),
            (_, 0) => "PUBLISHED RUN DONE".to_string(),
            (_, n) => format!("RUN DONE WITH {n} FAILED STEP(S)"),
        }
    );
    Ok(if failures == 0 {
        crate::EXIT_OK
    } else {
        crate::EXIT_ERROR
    })
}

fn do_pull(ctx: &Context, args: &PullArgs) -> Result<u8> {
    use cambium_bench::remote::BenchBox;
    let session = ctx.session(&Common {
        manifest: args.manifest.clone(),
        ..Default::default()
    })?;
    let subjects = match args.subjects.is_empty() {
        true => session.subject_names(None),
        false => args.subjects.clone(),
    };
    let bench_box = BenchBox {
        host: args.host.clone(),
        user: args.user.clone(),
        key: args.key.clone().unwrap_or_else(|| BenchBox::default().key),
        region: args.region.clone(),
        repo: args.box_repo.clone(),
        subjects: args.box_subjects.clone(),
        ..BenchBox::default()
    };
    let mut failed = 0u8;
    for s in &subjects {
        match bench_box.pull(s, &args.cache, !args.no_paths) {
            Ok(p) if p.cached => println!("{s}: cached: {}", args.cache.join(s).display()),
            Ok(p) => {
                println!("{s}: pulled into {}", args.cache.join(s).display());
                if !p.bases {
                    println!("  no archived base indexes for {s}; replaying against the pin");
                }
                if !p.heads {
                    println!("  no archived head index for {s}; the head check will be skipped");
                }
                if !args.no_paths && !p.paths {
                    println!("  no paths cache for {s}; pass --repo to replay");
                }
            }
            Err(e) => {
                eprintln!("cambium-bench: pull-results: {s}: {e:#}");
                failed = crate::EXIT_ERROR;
            }
        }
    }
    Ok(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &std::path::Path) -> Context {
        std::fs::write(
            dir.join("subject_repos.toml"),
            format!(
                "[[repo]]\nid = \"bazel\"\nurl = \"u\"\npin = \"{0}\"\n\
                 [[repo]]\nid = \"envoy\"\nurl = \"u\"\npin = \"{0}\"\n\
                 [[repo]]\nid = \"gone\"\nurl = \"u\"\npin = \"{0}\"\nskip = true\n",
                "a".repeat(40)
            ),
        )
        .unwrap();
        Context {
            root: dir.to_path_buf(),
            bazel_binary: None,
            format: crate::Format::Human,
            quiet: false,
            cambi: None,
            tools: Default::default(),
        }
    }

    fn args() -> RunArgs {
        RunArgs {
            subjects: vec![],
            steps: vec![],
            size: "published".into(),
            smoke: false,
            tools: vec![],
            restart_history: false,
            restart_changes: false,
            results_dir: None,
            require_sha: None,
            dry_run: true,
            manifest: None,
        }
    }

    /// `run` with no flags is the published run over the manifest, and the
    /// script's environment knobs are its flags.
    #[test]
    fn the_run_verb_composes_the_published_plan_from_flags() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let plan = plan_of(&ctx, &args()).unwrap();
        assert_eq!(
            plan.subjects,
            ["bazel", "envoy"],
            "skipped subjects are left out"
        );
        assert_eq!(plan.size, Size::Published);
        assert_eq!(plan.steps, StepKind::ALL);
        assert_eq!(plan.tools, ["bazel-diff", "tango"]);

        let plan = plan_of(
            &ctx,
            &RunArgs {
                subjects: vec!["envoy".into()],
                steps: vec!["changes".into(), "report".into()],
                smoke: true,
                restart_changes: true,
                ..args()
            },
        )
        .unwrap();
        assert_eq!(plan.subjects, ["envoy"]);
        assert_eq!(plan.size, Size::Smoke);
        assert_eq!(plan.steps, [StepKind::Changes, StepKind::Report]);
        assert!(plan.restart_changes && !plan.restart_history);
        let lines: Vec<String> = plan.steps().iter().map(|s| s.command_line()).collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[0].contains("--results-dir bench-results-smoke"),
            "{}",
            lines[0]
        );

        assert!(plan_of(
            &ctx,
            &RunArgs {
                steps: vec!["lunch".into()],
                ..args()
            }
        )
        .is_err());
        assert!(plan_of(
            &ctx,
            &RunArgs {
                size: "huge".into(),
                ..args()
            }
        )
        .is_err());
    }
}
