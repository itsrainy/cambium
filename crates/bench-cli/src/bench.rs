//! `cambium-bench` — the benchmark (docs/METHODOLOGY.md).
//!
//! The command tree is the vocabulary: `subjects` for what a run points at,
//! `history` and `changes` for the two workloads, `stages` and `sweep` for the
//! engine micro-benchmarks underneath them, `regress` to gate one result file
//! against another, and `report` to regenerate `docs/RESULTS.md`.
//!
//! Every subcommand takes a `<subject>` — a name from `subject_repos.toml`, or
//! `synth:<preset>` — and every one of them is idempotent and resumable: what
//! finished is recorded in `bench-results/<subject>/state.json`, and rerunning
//! picks up from there rather than redoing an hour of `bazel query`. Progress
//! is logged at `info`, so a long run is followable with `--log info`.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use cambium_bench::checkout::SubjectsDir;
use cambium_bench::manifest::{Manifest, RepoEntry};
use cambium_bench::state::State;
use cambium_bench::subject::Subject;
use cambium_bench::util::{cache_dir, versioned_name};
use cambium_bench::{changes, history};
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
    /// $CAMBIUM_SUBJECTS_DIR, else ~/.cache/cambium/subjects).
    #[arg(long)]
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

/// Everything a subcommand resolves once: the subject (with its Bazel binary
/// made absolute), the subjects directory, and where its results go.
struct Run {
    /// What the person named, kept so a checkout knows whether it may be
    /// produced on the spot (a synthetic subject) or must already exist.
    subject: Subject,
    entry: RepoEntry,
    subjects: SubjectsDir,
    /// `<root>/<subject>`: the state file and the cold indexes, shared by
    /// every workload.
    results: PathBuf,
    version: Option<String>,
    seed: u64,
    quick: bool,
}

impl Run {
    /// Loads the manifest and resolves `subject`, emitting the workspace first
    /// if it is a synthetic one.
    fn open(ctx: &Context, subject: &str, c: &Common) -> Result<Self> {
        let (path, manifest) = match &c.manifest {
            Some(p) => (p.clone(), Manifest::load(p)?),
            None => Manifest::discover(&ctx.root)?,
        };
        tracing::info!(manifest = %path.display(), %subject, "bench");
        let subjects = match &c.subjects_dir {
            Some(p) => SubjectsDir::new(p.clone()),
            None => SubjectsDir::from_env(),
        };
        let subject = Subject::parse(subject, &manifest)?;
        let mut entry = subject.entry(&subjects)?;
        let requested = ctx
            .bazel_binary
            .clone()
            .unwrap_or_else(|| entry.bazel.clone());
        entry.bazel = resolve_bazel(&requested)?;
        tracing::info!(bazel = %entry.bazel, "resolved the bazel binary");
        // Absolute, always. Tools and Bazel run as child processes with a
        // different working directory (the subject's worktree, a scratch
        // worktree), so a relative `--results-dir bench-results` would name a
        // different directory in each of them — which is exactly how
        // bazel-diff came to be handed a base worktree that did not exist.
        std::fs::create_dir_all(&c.results_dir)?;
        let results = cambium_bench::compare::absolute(&c.results_dir).join(&entry.id);
        std::fs::create_dir_all(&results)?;
        let results = cambium_bench::compare::absolute(&results);
        // The flag wins; the manifest's own pin is the fallback for a subject
        // that needs a specific Bazel and has no `.bazelversion` to say so.
        let version = c
            .bazel_version
            .clone()
            .or_else(|| entry.bazel_version.clone())
            .filter(|v| !v.is_empty());
        Ok(Self {
            subject,
            entry,
            subjects,
            results,
            version,
            seed: c.seed,
            quick: c.quick,
        })
    }

    /// The subject's worktree, emitted and cloned first if it is synthetic.
    fn checkout(&self) -> Result<cambium_bench::checkout::Checkout> {
        self.subjects.open_or_fetch(&self.subject, &self.entry)
    }

    /// The `cambi` this run measures, carrying the subject's bazelisk
    /// environment: a child `cambi` must run the same Bazel the rest of the
    /// comparison does, and on a subject whose `.bazeliskrc` routes through
    /// the Aspect CLI that is not the Bazel `bazelisk` would pick on its own.
    fn cambi(
        &self,
        ctx: &Context,
        checkout: &cambium_bench::checkout::Checkout,
    ) -> Result<cambium_bench::tools::cambi::Cambi> {
        Ok(ctx
            .cambi()?
            .clone()
            .with_env(cambium_bench::checkout::bazelisk_env(
                &checkout.dir,
                self.version(),
            )))
    }

    /// `bench-results/<subject>/<workload>/`, created.
    fn workload(&self, name: &str) -> Result<PathBuf> {
        let dir = self.results.join(name);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// The state file, as seen by this build: a state another cambium wrote
    /// (older index format, different configuration) is started over. The
    /// configuration is generated, not read from the worktree, so this costs
    /// no Bazel.
    fn state(&self) -> Result<State> {
        let cfg = self.subjects.config_for(&self.entry, self.version());
        State::load_for(
            &self.results,
            &self.entry.id,
            &self.entry.pin,
            &State::engine_id(&cfg.hash()),
        )
    }

    fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// `<stem>[-<version>].<ext>` — one result file per Bazel version, since
    /// the whole point of naming one is that two can disagree.
    fn result_path(&self, dir: &std::path::Path, stem: &str, ext: &str) -> PathBuf {
        dir.join(versioned_name(stem, self.version(), ext))
    }

    fn index_path(&self, sha: &str) -> PathBuf {
        self.result_path(&self.results, &format!("index-{sha}"), "bin")
    }

    /// The packages of the index's busiest targets, for the stress change
    /// sets. Needs the cold index at the pin, which is a `subjects build`.
    fn hub_packages(&self) -> Result<Vec<String>> {
        let state = self.state()?;
        let path = state
            .built
            .get(&self.entry.pin)
            .cloned()
            .unwrap_or_else(|| self.index_path(&self.entry.pin));
        anyhow::ensure!(
            path.exists(),
            "stress change sets need the index at the pin; run \
             `cambium-bench subjects build {}` first",
            self.entry.id
        );
        let data = cambium_index::IndexFile::open(&path)?.deserialize()?;
        let pkgs = changes::hub_packages(data.view());
        tracing::info!(hubs = pkgs.len(), "hub packages from the index");
        Ok(pkgs)
    }
}

/// Selects `run`'s change sets and records them under `<subject>/changes/`.
///
/// The one selector: `cambium-bench change-sets` and `cambium-bench changes` used to have a
/// copy each, which is how only the first of them came to pass `hub_packages`
/// -- so a `--kind stress` comparison silently drew its change sets from an
/// empty hub list and stressed nothing.
fn select_change_sets(
    run: &Run,
    kinds: &[String],
    cap: usize,
    stress_cap: usize,
) -> Result<(PathBuf, changes::ChangeSetSet)> {
    let explicit = !kinds.is_empty();
    let kinds = parse_kinds(kinds)?;
    let checkout = run.checkout()?;
    let (paths, globs) = run.entry.invalidator_rules();
    let opts = changes::ChangeSetOptions {
        repo_id: run.entry.id.clone(),
        pin: run.entry.pin.clone(),
        history_window: run.entry.history_window,
        seed: run.seed,
        cap: stress_cap,
        total_cap: Some(cap),
        // Only when a stress change set is actually wanted: the hubs cost an
        // index open, and every other kind ignores them.
        //
        // Best effort when `stress` came from the default set of kinds rather
        // than from a `--kind stress` the person typed: three of the four
        // stress categories (`bzl`, `build_add_delete`, `invalidator`) need no
        // index at all, and refusing to select any of them because
        // `subjects build` has not run yet would make the default
        // `bench changes` unrunnable on a fresh subject. Asking for stress
        // explicitly and silently getting no hub change sets is the other
        // failure, so that one is an error.
        hub_packages: match kinds.contains(&changes::ChangeSetKind::Stress) {
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
    let path = run.result_path(
        &run.workload("changes")?,
        &format!("change-sets-{}", run.entry.pin),
        "json",
    );
    set.write(&path)?;
    Ok((path, set))
}

/// A Bazel binary the benchmark can run: an explicit path as given, otherwise
/// a PATH lookup, otherwise the bazelisk the suite installs for itself.
fn resolve_bazel(binary: &str) -> Result<String> {
    if binary.contains('/') {
        let p = expand_tilde(binary);
        anyhow::ensure!(p.exists(), "no bazel binary at {}", p.display());
        return Ok(p.display().to_string());
    }
    if let Some(found) = which(binary) {
        return Ok(found.display().to_string());
    }
    let fallback = cache_dir("bin").join(binary);
    if fallback.exists() {
        return Ok(fallback.display().to_string());
    }
    anyhow::bail!(
        "no {binary} on PATH or in {}; pass --bazel-binary",
        cache_dir("bin").display()
    )
}

fn which(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(binary))
        .find(|c| c.is_file())
}

fn expand_tilde(s: &str) -> PathBuf {
    match s.strip_prefix("~/") {
        Some(rest) => cambium_bench::util::home().join(rest),
        None => PathBuf::from(s),
    }
}

/// The preset name behind a `synth:<preset>` subject, for the stage benchmarks
/// — which time a generated graph directly and never go near a workspace.
fn preset_of(subject: &str, what: &str) -> Result<String> {
    subject
        .strip_prefix(cambium_bench::subject::SYNTH_PREFIX)
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "`bench {what} {subject}`: {what} runs on a synthetic subject \
                 (synth:<preset>); a real subject has no generated graph to \
                 measure a stage over"
            )
        })
}

/// `cambium-bench labels`: the stored sets of one change set, filtered.
///
/// Deliberately does not go through [`Run::open`]: nothing here runs a Bazel
/// or opens a checkout, and refusing to show a stored file because no bazel
/// binary is installed would defeat the point of having written it — these
/// are read on a laptop, from results tarred off the benchmark machine.
#[allow(clippy::too_many_arguments)]
fn do_labels(
    ctx: &Context,
    subject: &str,
    change_set: &str,
    common: &Common,
    tool: Option<String>,
    class: Option<String>,
    side: Option<String>,
    diff: bool,
) -> Result<u8> {
    use cambium_bench::labels;

    let (_, manifest) = match &common.manifest {
        Some(p) => (p.clone(), Manifest::load(p)?),
        None => Manifest::discover(&ctx.root)?,
    };
    let subjects = match &common.subjects_dir {
        Some(p) => SubjectsDir::new(p.clone()),
        None => SubjectsDir::from_env(),
    };
    let entry = Subject::parse(subject, &manifest)?.entry(&subjects)?;
    let changes = common.results_dir.join(&entry.id).join("changes");
    let dir = labels::resolve(&changes, change_set)?;

    if diff {
        for d in labels::diff(&dir)? {
            println!(
                "{}\tonly-tool {}\tonly-cambium {}\tboth {}",
                d.tool, d.only_tool, d.only_cambium, d.both
            );
        }
        return Ok(crate::EXIT_OK);
    }

    let filter = labels::Filter {
        tool: tool.clone(),
        class: class
            .as_deref()
            .map(|c| {
                labels::parse_class(c)
                    .ok_or_else(|| anyhow::anyhow!("no disagreement class named {c}"))
            })
            .transpose()?,
        side: side
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
        for row in filter.apply(&labels::read_classes(&path)?) {
            println!(
                "{}\t{}\t{}\t{}",
                row.tool,
                labels::side_name(row.side),
                row.class.name(),
                row.label
            );
        }
        return Ok(crate::EXIT_OK);
    }

    let sets = labels::stored_sets(&dir)?;
    let wanted: Vec<_> = match &tool {
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
    // Named only when there is more than one of them: a `--tool` answer is
    // meant to be pipeable into `comm` or `grep -f`.
    let header = wanted.len() > 1;
    for (name, path) in wanted {
        if header {
            println!("# {name}");
        }
        for label in labels::read_set(&path)? {
            println!("{label}");
        }
    }
    Ok(crate::EXIT_OK)
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    match args {
        Args::Subjects { what } => do_subjects(ctx, what),
        Args::Synth(args) => crate::synth::run(ctx, args),
        Args::Replay(args) => crate::replay::run(ctx, *args),
        Args::History {
            subject,
            commits,
            oracles,
            restart,
            common,
        } => {
            let run = Run::open(ctx, &subject, &common)?;
            let commits = commits.or(run.quick.then_some(cambium_bench::QUICK_COMMITS));
            do_history(ctx, &run, commits, &oracles, !restart)?;
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
            let run = Run::open(ctx, &subject, &common)?;
            let cap = change_sets
                .or(run.quick.then_some(cambium_bench::QUICK_CHANGE_SETS))
                .unwrap_or(cambium_bench::DEFAULT_CHANGE_SETS);
            if select_only {
                do_change_sets(&run, &kinds, cap, stress_cap, ctx.format.data())?;
                return Ok(crate::EXIT_OK);
            }
            do_compare(
                ctx,
                &run,
                &CompareArgs {
                    change_sets: change_set_file,
                    kinds,
                    oracle_sample,
                    oracles,
                    tools,
                    cap,
                    stress_cap,
                    restart,
                },
            )
        }
        Args::Labels {
            subject,
            change_set,
            tool,
            class,
            side,
            diff,
            common,
        } => do_labels(ctx, &subject, &change_set, &common, tool, class, side, diff),
        Args::Stages {
            subject,
            stages,
            runs,
            out,
            markdown,
            stdout,
            common,
        } => {
            let preset = preset_of(&subject, "stages")?;
            let dir = stage_dir(&subject, &common, "stages")?;
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
            let preset = preset_of(&subject, "sweep")?;
            let dir = stage_dir(&subject, &common, "sweep")?;
            let values = values
                .iter()
                .map(|v| parse_size(v))
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
        } => do_report(ctx, subject.as_deref(), doc.as_deref(), &common),
    }
}

/// `bench-results/<subject>/<workload>/` for a stage benchmark, which needs no
/// checkout and so does not go through [`Run::open`].
fn stage_dir(subject: &str, c: &Common, workload: &str) -> Result<PathBuf> {
    let id = subject.replace(':', "-");
    let dir = cambium_bench::compare::absolute(&c.results_dir)
        .join(id)
        .join(workload);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn timestamped() -> String {
    format!("{}.json", cambium_bench::util::now_unix())
}

/// Accepts `1000`, `1e6`, `2M`, `500k`.
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('k') | Some('K') => (&s[..s.len() - 1], 1_000.0),
        Some('m') | Some('M') => (&s[..s.len() - 1], 1_000_000.0),
        Some('g') | Some('G') => (&s[..s.len() - 1], 1_000_000_000.0),
        _ => (s, 1.0),
    };
    let v: f64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("cannot parse size {s:?}"))?;
    Ok((v * mult) as u64)
}

fn do_subjects(ctx: &Context, what: SubjectArgs) -> Result<u8> {
    match what {
        SubjectArgs::List { common: c } => {
            let (path, manifest) = match &c.manifest {
                Some(p) => (p.clone(), Manifest::load(p)?),
                None => Manifest::discover(&ctx.root)?,
            };
            println!("real subjects ({}):", path.display());
            for e in &manifest.repos {
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
            for name in subject_names(ctx, subject, &c)? {
                let run = Run::open(ctx, &name, &c)?;
                fetch(&run)?;
            }
            Ok(crate::EXIT_OK)
        }
        SubjectArgs::Build {
            subject,
            at,
            force,
            common: c,
        } => {
            for name in subject_names(ctx, subject, &c)? {
                let run = Run::open(ctx, &name, &c)?;
                build(ctx, &run, at.as_deref(), force)?;
            }
            Ok(crate::EXIT_OK)
        }
    }
}

/// One named subject, or every manifest subject that is not skipped.
fn subject_names(ctx: &Context, subject: Option<String>, c: &Common) -> Result<Vec<String>> {
    if let Some(s) = subject {
        return Ok(vec![s]);
    }
    let (_, manifest) = match &c.manifest {
        Some(p) => (p.clone(), Manifest::load(p)?),
        None => Manifest::discover(&ctx.root)?,
    };
    Ok(manifest
        .repos
        .iter()
        .filter(|e| !e.skip)
        .map(|e| e.id.clone())
        .collect())
}

fn fetch(run: &Run) -> Result<()> {
    let mut state = run.state()?;
    tracing::info!(id = %run.entry.id, url = %run.entry.url, pin = %run.entry.pin, "fetching");
    let checkout = run
        .subjects
        .fetch(&run.entry)
        .with_context(|| format!("fetching subject {}", run.entry.id))?;
    let cfg = checkout.write_config(&run.entry, run.version())?;
    state.fetched = true;
    state.save(&run.results)?;
    tracing::info!(worktree = %checkout.dir.display(), config = %cfg.display(), "fetched");
    println!("{}", checkout.dir.display());
    Ok(())
}

fn build(ctx: &Context, run: &Run, at: Option<&str>, force: bool) -> Result<PathBuf> {
    let checkout = run.checkout()?;
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
                    println!("{}", prev.display());
                    return Ok(prev.clone());
                }
                Err(e) => tracing::warn!(path = %prev.display(), error = %e, "rebuilding"),
            }
        }
    }
    checkout.checkout(&sha)?;
    // The configuration the subject's runs share has to be on disk before a
    // child `cambi` can read it; the engine handle itself is only opened for
    // the compression setting and the store the rest of the workload uses.
    checkout.write_config(&run.entry, run.version())?;
    let cambi = run.cambi(ctx, &checkout)?;
    tracing::info!(%sha, cambi = %cambi.version(), "cold build: cambi index build (this takes a while)");
    let wall = cambi.index_build(&checkout.dir, &sha, &out, true)?;
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
    if let Some(w) = cambium_engine::inspect::PartialLoad::from(v).warning() {
        tracing::warn!("{w}");
        eprintln!("cambium-bench: WARNING: {w}");
    }
    state.built.insert(sha, out.clone());
    state.save(&run.results)?;
    println!("{}", out.display());
    Ok(out)
}

fn do_history(
    ctx: &Context,
    run: &Run,
    commits: Option<usize>,
    oracles: &[String],
    resume: bool,
) -> Result<()> {
    let rebuild_oracle = parse_history_oracles(oracles)?;
    let checkout = run.checkout()?;
    let engine = checkout.engine(&run.entry, run.version())?;
    let cambi = run.cambi(ctx, &checkout)?;
    let results = run.workload("history")?;
    let range = commits.unwrap_or(run.entry.history_window);
    let commits = changes::range_commits(&checkout.repo, &run.entry.pin, range)?;
    tracing::info!(
        commits = commits.len(),
        from = %commits.first().cloned().unwrap_or_default(),
        to = %run.entry.pin,
        "running the history"
    );
    let opts = history::HistoryOptions {
        repo_id: run.entry.id.clone(),
        pin: run.entry.pin.clone(),
        results_dir: results.clone(),
        state_dir: run.results.clone(),
        publish: true,
        resume,
        bazel_version: run.version().unwrap_or_default().to_string(),
        diff_cap: 50,
        rebuild_oracle,
        cambi_version: Some(cambi.version().to_string()),
    };
    let driver = history::CambiDriver {
        checkout: &checkout,
        cambi: &cambi,
    };
    let result = history::history(&engine, &driver, &commits, &opts)?;
    let path = history::HistoryResult::path_versioned(&results, &run.entry.pin, run.version());
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
    println!("{}", path.display());
    if let Some(o) = &result.oracle {
        if !o.ok {
            anyhow::bail!("history oracle failed: {}", o.diff.summary());
        }
    }
    Ok(())
}

/// `rebuild`, or `none`.
fn parse_history_oracles(oracles: &[String]) -> Result<bool> {
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

/// `rdeps` and `load`, in any order, or `none`.
fn parse_change_oracles(oracles: &[String]) -> Result<(bool, bool)> {
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

fn do_change_sets(
    run: &Run,
    kinds: &[String],
    cap: usize,
    stress_cap: usize,
    output: Option<DataFormat>,
) -> Result<()> {
    let (path, set) = select_change_sets(run, kinds, cap, stress_cap)?;
    tracing::info!(change_sets = set.change_sets.len(), path = %path.display(), "selected the change sets");
    let Some(output) = output else {
        return Ok(());
    };
    match output {
        DataFormat::Json => println!("{}", serde_json::to_string_pretty(&set)?),
        DataFormat::Jsonl => println!("{}", crate::result_event(&set)?),
        DataFormat::Text => {
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

/// What `bench changes` was asked to do.
#[derive(Clone, Debug)]
struct CompareArgs {
    change_sets: Option<PathBuf>,
    kinds: Vec<String>,
    oracle_sample: usize,
    oracles: Vec<String>,
    tools: Vec<String>,
    cap: usize,
    stress_cap: usize,
    restart: bool,
}

impl CompareArgs {
    /// The tool names to run, before any of them is looked for on disk.
    ///
    /// `--tools` is the exact list when given; otherwise the default is
    /// "both tools" — a bazel-diff (`--bazel-diff rust|kotlin|none`) and
    /// Tango (`--no-tango` to drop it) — because a comparison against one tool
    /// only tells you which of the two disagrees with cambium, not which of
    /// the three is the odd one out.
    fn wanted(&self) -> Result<Vec<String>> {
        let names: Vec<&str> = match self.tools.is_empty() {
            true => vec!["bazel-diff", "tango"],
            false => self.tools.iter().map(String::as_str).collect(),
        };
        let mut out = Vec::new();
        for n in names {
            match n.trim() {
                // cambium is not a foil: it is what the foils are compared
                // against, and it runs on every change set whether or not it
                // is named. Naming it alone is how a run measures cambium with
                // no tool to disagree with it.
                "cambium" => {}
                "bazel-diff" | "bazel-diff-rust" | "rust" => out.push(
                    cambium_bench::tools::bazeldiff::Implementation::Rust
                        .name()
                        .to_string(),
                ),
                "bazel-diff-kotlin" | "kotlin" => out.push(
                    cambium_bench::tools::bazeldiff::Implementation::Kotlin
                        .name()
                        .to_string(),
                ),
                "tango" => out.push(cambium_bench::tools::tango::NAME.to_string()),
                "none" | "" => {}
                other => anyhow::bail!(
                    "unknown tool {other:?}: cambium, bazel-diff, bazel-diff-kotlin, tango"
                ),
            }
        }
        Ok(out)
    }
}

/// Locates one tool by name, or explains why it is being skipped.
///
/// A missing binary is a warning, not an error: the machine that runs the
/// nightly benchmark job may have bazel-diff and no Go toolchain, and a run that
/// refuses to compare anything because one of two optional tools is absent is
/// worse than a run that says so and compares the other.
fn open_tool(name: &str) -> Result<Option<Box<dyn cambium_bench::tools::Tool>>> {
    use cambium_bench::tools::bazeldiff::{BazelDiff, Implementation};
    use cambium_bench::tools::tango::Tango;
    let found: Result<Box<dyn cambium_bench::tools::Tool>> = match name {
        "tango" => Tango::discover().map(|t| Box::new(t) as _),
        "bazel-diff-rust" | "rust" => {
            BazelDiff::discover(Implementation::Rust).map(|b| Box::new(b) as _)
        }
        "bazel-diff-kotlin" | "kotlin" => {
            BazelDiff::discover(Implementation::Kotlin).map(|b| Box::new(b) as _)
        }
        other => {
            anyhow::bail!("unknown tool {other:?}: bazel-diff-rust, bazel-diff-kotlin, tango")
        }
    };
    match found {
        Ok(p) => {
            tracing::info!(tool = p.name(), version = %p.version(), "tool");
            Ok(Some(p))
        }
        Err(e) => {
            eprintln!("cambium-bench: changes: skipping {name}: {e:#}");
            Ok(None)
        }
    }
}

fn do_compare(ctx: &Context, run: &Run, args: &CompareArgs) -> Result<u8> {
    let wanted = args.wanted()?;
    let owned: Vec<Box<dyn cambium_bench::tools::Tool>> = wanted
        .iter()
        .map(|n| open_tool(n))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    if owned.is_empty() && !wanted.is_empty() {
        eprintln!("cambium-bench: changes: no tool is installed; nothing to compare against");
        return Ok(crate::EXIT_ERROR);
    }
    let tools: Vec<&dyn cambium_bench::tools::Tool> = owned.iter().map(|p| p.as_ref()).collect();

    let checkout = run.checkout()?;
    let set = load_change_sets(run, args)?;
    let change_sets: Vec<_> = set
        .change_sets
        .iter()
        .take(args.cap.max(1))
        .cloned()
        .collect();
    anyhow::ensure!(!change_sets.is_empty(), "no change sets to compare");

    let engine = checkout.engine(&run.entry, run.version())?;
    let (rdeps, load) = parse_change_oracles(&args.oracles)?;
    let results = run.workload("changes")?;
    let opts = cambium_bench::compare::ChangesOptions {
        bazel_version: run.version.clone(),
        rdeps_oracle: rdeps,
        oracle_k: args.oracle_sample,
        load_oracle: load,
        seed: run.seed,
        resume: !args.restart,
        scratch: run.results.join("scratch"),
        state_dir: run.results.clone(),
        tango_change_sets: run.quick.then_some(cambium_bench::QUICK_TANGO_CHANGE_SETS),
        ..cambium_bench::compare::ChangesOptions::new(
            run.entry.id.clone(),
            run.entry.pin.clone(),
            results,
        )
    };
    tracing::info!(
        change_sets = change_sets.len(),
        tools = tools.len(),
        oracle_k = opts.oracle_k,
        tango_change_sets = opts.tango_change_sets.unwrap_or(change_sets.len()),
        "comparing"
    );
    let result = cambium_bench::compare::compare_change_sets(
        &engine,
        &run.cambi(ctx, &checkout)?,
        &checkout,
        &change_sets,
        &tools,
        &opts,
    )?;
    print_compare_summary(&result);
    println!("{}", opts.output_path().display());
    Ok(if result.summary.pass {
        crate::EXIT_OK
    } else {
        crate::EXIT_ERROR
    })
}

/// The change sets to compare: an explicit file, else the one a previous run wrote
/// for this pin, else a fresh selection with the requested kinds.
fn load_change_sets(run: &Run, args: &CompareArgs) -> Result<changes::ChangeSetSet> {
    if let Some(p) = &args.change_sets {
        return changes::ChangeSetSet::read(p).with_context(|| format!("reading {}", p.display()));
    }
    let default = run.result_path(
        &run.workload("changes")?,
        &format!("change-sets-{}", run.entry.pin),
        "json",
    );
    if args.kinds.is_empty() && default.exists() {
        tracing::info!(path = %default.display(), "using the recorded change sets");
        return changes::ChangeSetSet::read(&default);
    }
    Ok(select_change_sets(run, &args.kinds, args.cap, args.stress_cap)?.1)
}

fn parse_kinds(kinds: &[String]) -> Result<Vec<changes::ChangeSetKind>> {
    if kinds.is_empty() {
        return Ok(vec![
            changes::ChangeSetKind::Pr,
            changes::ChangeSetKind::Adjacent,
            changes::ChangeSetKind::Stress,
        ]);
    }
    kinds
        .iter()
        .map(|k| {
            changes::ChangeSetKind::parse(k).ok_or_else(|| {
                anyhow::anyhow!("unknown change-set kind {k:?}: pr, adjacent, stress")
            })
        })
        .collect()
}

/// The table a person reads before opening the JSON.
fn print_compare_summary(result: &cambium_bench::compare::ChangesResult) {
    let s = &result.summary;
    println!(
        "\n{} {}  change sets {}  passed {}  global {}  min recall {:.1}%",
        result.repo,
        &result.pin[..result.pin.len().min(12)],
        s.change_sets,
        s.passed,
        s.globals,
        s.min_recall * 100.0
    );
    println!(
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
        println!("{name:<20} {:>9.1}% {:>9} ms {warm:>12}", p * 100.0, cold);
    }
    println!(
        "{:<20} {:>10} {:>9} ms {:>9} ms",
        "cambium", "—", s.cambium_cold.p50_ms, s.cambium_warm.p50_ms
    );
    let failing: Vec<_> = s
        .by_class
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(k, n)| format!("{k}={n}"))
        .collect();
    println!(
        "disagreements: {}",
        if failing.is_empty() {
            "none".to_string()
        } else {
            failing.join(" ")
        }
    );
}
/// `docs/RESULTS.md`: each subject's section, plus the top table and the
/// synthetic headline, rebuilt from every subject that has results.
fn do_report(
    ctx: &Context,
    subject: Option<&str>,
    doc: Option<&std::path::Path>,
    c: &Common,
) -> Result<u8> {
    let doc = doc
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| ctx.root.join("docs/RESULTS.md"));
    let (_, manifest) = match &c.manifest {
        Some(p) => (p.clone(), Manifest::load(p)?),
        None => Manifest::discover(&ctx.root)?,
    };
    let subjects = match &c.subjects_dir {
        Some(p) => SubjectsDir::new(p.clone()),
        None => SubjectsDir::from_env(),
    };
    let parent = cambium_bench::compare::absolute(&c.results_dir);

    // Every subject at once, not just the named one: a per-subject nightly job
    // rewrites the top table and the synthetic headline wholesale, and must not
    // drop its neighbours' rows on the way.
    let results = cambium_bench::report::SubjectResults::collect(&manifest, &parent, &subjects)?;
    if results.is_empty() {
        eprintln!(
            "cambium-bench: report: no history-*.json or changes-*.json under {}",
            parent.display()
        );
        return Ok(crate::EXIT_ERROR);
    }
    let only = subject.map(|s| s.replace(':', "-"));
    if let Some(id) = &only {
        anyhow::ensure!(
            results.iter().any(|r| &r.id == id),
            "no results for {id} under {}",
            parent.display()
        );
    }

    let bench = cambium_bench::report::BenchLegacy::read(
        &ctx.root.join("docs/benchmarks/sweep-legacy.json"),
    );
    for r in &results {
        if only.as_ref().is_some_and(|id| &r.id != id) {
            continue;
        }
        cambium_bench::report::update_doc(
            &doc,
            &r.id,
            &cambium_bench::report::render_subject_section(r, None),
        )?;
    }
    cambium_bench::report::update_doc(
        &doc,
        "top",
        &cambium_bench::report::render_top_table(&results),
    )?;
    cambium_bench::report::update_doc(
        &doc,
        "synthetic",
        &cambium_bench::report::render_bench_section(&bench, Some("bench-legacy.svg")),
    )?;
    tracing::info!(doc = %doc.display(), subjects = results.len(), "report written");
    println!("{}", doc.display());
    Ok(crate::EXIT_OK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_manifest_is_discovered_upward_from_the_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(
            dir.path().join("subject_repos.toml"),
            format!(
                "[[repo]]\nid = \"bazel\"\nurl = \"u\"\npin = \"{}\"\n",
                "a".repeat(40)
            ),
        )
        .unwrap();
        let (path, m) = Manifest::discover(&deep).unwrap();
        assert_eq!(path, dir.path().join("subject_repos.toml"));
        assert_eq!(m.get("bazel").unwrap().history_window, 300);
        assert!(Manifest::discover(Path::new("/")).is_err());
    }
}
