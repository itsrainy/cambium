//! `cambi corpus` — the real-repository validation suite (spec §3-§6).
//!
//! Every subcommand is idempotent and resumable: what finished is recorded in
//! `bench-results/<id>/state.json`, and rerunning picks up from there rather
//! than redoing an hour of `bazel query`. Progress is logged at `info`, so a
//! long run is followable with `--log info`.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use cambium_bench::manifest::{Manifest, RepoEntry};
use cambium_bench::{changes, history, State, SubjectsDir};
use cambium_engine::inspect::{IndexQuery, IndexStats};
use clap::{Args as ClapArgs, Subcommand};

use crate::cmd::Context;
use crate::DataFormat;

/// Flags every corpus subcommand shares.
#[derive(ClapArgs, Clone)]
pub struct Common {
    /// Corpus repository id, as in subject_repos.toml.
    pub id: String,
    /// Path to subject_repos.toml (default: discovered upward from the cwd).
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    /// Where results go: <results-dir>/<id>/.
    #[arg(long, default_value = "bench-results")]
    pub results_dir: PathBuf,
    /// Mirrors, worktrees and caches (default: $CAMBIUM_CORPUS_DIR, else
    /// ~/.cache/cambium/corpus).
    #[arg(long)]
    pub corpus_dir: Option<PathBuf>,
    /// Bazel version for bazelisk (USE_BAZEL_VERSION); also suffixes the
    /// result file names. Default: whatever the repo's .bazelversion says.
    #[arg(long)]
    pub bazel_version: Option<String>,
    /// Seed for the deterministic change set sampling.
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
}

#[derive(Subcommand)]
pub enum Args {
    /// Mirror the repository and check out its pin.
    Fetch {
        #[command(flatten)]
        common: Common,
    },
    /// Cold-build an index and publish it to the corpus store.
    Build {
        #[command(flatten)]
        common: Common,
        /// Revision to build. Defaults to the manifest pin.
        #[arg(long)]
        at: Option<String>,
        /// Rebuild even if this revision was already built.
        #[arg(long)]
        force: bool,
    },
    /// History the updater over the first-parent range ending at the pin.
    History {
        #[command(flatten)]
        common: Common,
        /// First-parent commits before the pin. Defaults to the manifest's.
        #[arg(long)]
        range: Option<usize>,
        /// Start over rather than resuming the recorded history.
        #[arg(long)]
        restart: bool,
    },
    /// Select the deterministic (base, head) comparison change sets.
    Pairs {
        #[command(flatten)]
        common: Common,
        /// adjacent, pr, or stress. Repeatable; default: all three.
        #[arg(long = "kind")]
        kinds: Vec<String>,
        /// Stress change sets per category.
        #[arg(long, default_value_t = 20)]
        cap: usize,
        #[arg(long, value_enum, default_value_t = DataFormat::Text)]
        output: DataFormat,
    },
    /// Three-way comparison against bazel-diff, Tango, and the rdeps oracle.
    Compare {
        #[command(flatten)]
        common: Common,
        /// A change sets JSON to compare (default: the one `corpus change_sets` wrote, or
        /// a fresh selection).
        #[arg(long)]
        change_sets: Option<PathBuf>,
        /// ChangeSet kinds to select when there is no change sets file. Repeatable.
        #[arg(long = "kind")]
        kinds: Vec<String>,
        /// Roots the rdeps oracle samples per change set; 0 disables it.
        #[arg(long, default_value_t = 100)]
        oracle_sample: usize,
        /// Which bazel-diff to run: rust, kotlin, or none.
        #[arg(long, default_value = "rust")]
        bazel_diff: String,
        /// Run Tango. It is in the default set already; pass this to add it
        /// to an explicit `--tools` list.
        #[arg(long)]
        tango: bool,
        /// Do not run Tango.
        #[arg(long, conflicts_with = "tango")]
        no_tango: bool,
        /// The exact tool list, overriding the two flags above:
        /// `bazel-diff-rust`, `bazel-diff-kotlin`, `tango`.
        #[arg(long, value_delimiter = ',')]
        tools: Vec<String>,
        /// Stress change sets per category when selecting, and the cap on how many
        /// change sets are compared in one run.
        #[arg(long, default_value_t = cambium_bench::DEFAULT_CHANGE_SETS)]
        cap: usize,
        /// Compare every change set again rather than resuming.
        #[arg(long)]
        restart: bool,
    },
    /// Regenerate this repo's section of docs/CORPUS.md.
    Report {
        #[command(flatten)]
        common: Common,
        /// The document to update.
        #[arg(long)]
        doc: Option<PathBuf>,
    },
    /// fetch, build, history, change sets, compare, report.
    All {
        #[command(flatten)]
        common: Common,
    },
}

impl Args {
    fn common(&self) -> &Common {
        match self {
            Args::Fetch { common }
            | Args::Build { common, .. }
            | Args::History { common, .. }
            | Args::Pairs { common, .. }
            | Args::Compare { common, .. }
            | Args::Report { common, .. }
            | Args::All { common } => common,
        }
    }
}

/// Everything the subcommands resolve once: the manifest entry (with its
/// Bazel binary made absolute), the corpus directory, and the results dir.
struct Run {
    entry: RepoEntry,
    /// The whole manifest, not just this entry: the report's top table is
    /// rebuilt from every entry that has results, not only this one.
    manifest: Manifest,
    corpus: SubjectsDir,
    /// `--results-dir` itself, absolute: the parent every repository's results
    /// hang off, which the report walks to rebuild the cross-repo tables.
    results_root: PathBuf,
    /// This run's own directory: `<root>/<id>`, or `<root>/<id>/quick`.
    results: PathBuf,
    version: Option<String>,
    seed: u64,
}

impl Run {
    fn open(ctx: &Context, c: &Common) -> Result<Self> {
        let (path, manifest) = match &c.manifest {
            Some(p) => (p.clone(), Manifest::load(p)?),
            None => Manifest::discover(&ctx.root)?,
        };
        tracing::info!(manifest = %path.display(), id = %c.id, "corpus");
        let mut entry = manifest.get(&c.id)?.clone();
        let requested = ctx
            .bazel_binary
            .clone()
            .unwrap_or_else(|| entry.bazel.clone());
        entry.bazel = resolve_bazel(&requested)?;
        tracing::info!(bazel = %entry.bazel, "resolved the bazel binary");
        let corpus = match &c.corpus_dir {
            Some(p) => SubjectsDir::new(p.clone()),
            None => SubjectsDir::from_env(),
        };
        // Absolute, always. Tools and Bazel run as child processes with a
        // different working directory (the corpus worktree, a scratch
        // worktree), so a relative `--results-dir bench-results` would name a
        // different directory in each of them — which is exactly how
        // bazel-diff came to be handed a base worktree that did not exist.
        std::fs::create_dir_all(&c.results_dir)?;
        let results_root = cambium_bench::compare::absolute(&c.results_dir);
        // A quick run's files sit in their own directory so they can never be
        // read as, or write over, the published full-profile numbers.
        let results = results_root.join(&c.id);
        std::fs::create_dir_all(&results)?;
        let results = cambium_bench::compare::absolute(&results);
        // The flag wins; the manifest's own pin is the fallback for a repo
        // that needs a specific Bazel and has no `.bazelversion` to say so.
        let version = c
            .bazel_version
            .clone()
            .or_else(|| entry.bazel_version.clone())
            .filter(|v| !v.is_empty());
        Ok(Self {
            entry,
            manifest,
            corpus,
            results_root,
            results,
            version,
            seed: c.seed,
        })
    }

    /// The state file, as seen by this build: a state another cambium wrote
    /// (older index format, different configuration) is started over. The
    /// configuration is generated, not read from the worktree, so this costs
    /// no Bazel.
    fn state(&self) -> Result<State> {
        let cfg = self.corpus.config_for(&self.entry, self.version());
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
    /// the whole point of the matrix is that the two can disagree.
    fn result_path(&self, stem: &str, ext: &str) -> PathBuf {
        self.results.join(suffixed(stem, self.version(), ext))
    }

    fn index_path(&self, sha: &str) -> PathBuf {
        self.result_path(&format!("index-{sha}"), "bin")
    }
}

/// `<stem>[-<version>].<ext>`, with anything path-hostile in the version
/// flattened (`7.x` is fine, `latest/rc` would not be).
pub fn suffixed(stem: &str, version: Option<&str>, ext: &str) -> String {
    match version.filter(|v| !v.is_empty()) {
        Some(v) => format!("{stem}-{}.{ext}", v.replace(['/', ' ', ':'], "_")),
        None => format!("{stem}.{ext}"),
    }
}

/// A Bazel binary the corpus can run: an explicit path as given, otherwise a
/// PATH lookup, otherwise the bazelisk the suite installs for itself.
fn resolve_bazel(binary: &str) -> Result<String> {
    if binary.contains('/') {
        let p = expand_tilde(binary);
        anyhow::ensure!(p.exists(), "no bazel binary at {}", p.display());
        return Ok(p.display().to_string());
    }
    if let Some(found) = which(binary) {
        return Ok(found.display().to_string());
    }
    let fallback = home().join(".cache/cambium/bin").join(binary);
    if fallback.exists() {
        return Ok(fallback.display().to_string());
    }
    anyhow::bail!(
        "no {binary} on PATH or in {}; pass --bazel-binary",
        home().join(".cache/cambium/bin").display()
    )
}

fn which(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(binary))
        .find(|c| c.is_file())
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

fn expand_tilde(s: &str) -> PathBuf {
    match s.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(s),
    }
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    let run = Run::open(ctx, args.common())?;
    match args {
        Args::Fetch { .. } => {
            fetch(&run)?;
            Ok(crate::EXIT_OK)
        }
        Args::Build { at, force, .. } => {
            build(&run, at.as_deref(), force)?;
            Ok(crate::EXIT_OK)
        }
        Args::History { range, restart, .. } => {
            do_replay(&run, range, !restart)?;
            Ok(crate::EXIT_OK)
        }
        Args::Pairs {
            kinds, cap, output, ..
        } => {
            do_change_sets(&run, &kinds, cap, output)?;
            Ok(crate::EXIT_OK)
        }
        Args::Compare {
            change_sets,
            kinds,
            oracle_sample,
            bazel_diff,
            tango,
            no_tango,
            tools,
            cap,
            restart,
            ..
        } => do_compare(
            &run,
            &CompareArgs {
                change_sets,
                kinds,
                oracle_sample,
                bazel_diff,
                tango,
                no_tango,
                tools,
                cap,
                restart,
            },
        ),
        Args::Report { doc, .. } => do_report(ctx, &run, doc.as_deref()),
        Args::All { .. } => {
            fetch(&run)?;
            build(&run, None, false)?;
            do_replay(&run, None, true)?;
            do_change_sets(
                &run,
                &[],
                cambium_bench::DEFAULT_CHANGE_SETS,
                DataFormat::Text,
            )?;
            let code = do_compare(&run, &CompareArgs::default())?;
            if code != crate::EXIT_OK {
                return Ok(code);
            }
            do_report(ctx, &run, None)
        }
    }
}

fn fetch(run: &Run) -> Result<()> {
    let mut state = run.state()?;
    tracing::info!(id = %run.entry.id, url = %run.entry.url, pin = %run.entry.pin, "fetching");
    let checkout = run
        .corpus
        .fetch(&run.entry)
        .with_context(|| format!("fetching corpus repo {}", run.entry.id))?;
    let cfg = checkout.write_config(&run.entry, run.version())?;
    state.fetched = true;
    state.save(&run.results)?;
    tracing::info!(worktree = %checkout.dir.display(), config = %cfg.display(), "fetched");
    println!("{}", checkout.dir.display());
    Ok(())
}

fn build(run: &Run, at: Option<&str>, force: bool) -> Result<PathBuf> {
    let checkout = run.corpus.open(&run.entry)?;
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
    let engine = checkout.engine(&run.entry, run.version())?;
    tracing::info!(%sha, "cold build: bazel query //...:all-targets (this takes a while)");
    let t = std::time::Instant::now();
    let index = engine.full_build(&sha)?;
    let wall = t.elapsed();
    cambium_index::format::write_index(&out, &index)?;
    let c = &index.header.counts;
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
    if let Some(w) = cambium_engine::inspect::PartialLoad::from(index.view()).warning() {
        tracing::warn!(
            packages_in_error = index.provenance.packages_in_error,
            first = ?index.provenance.error_packages,
            "{w}"
        );
        eprintln!("cambium: WARNING: {w}");
    }
    for n in engine.publish_base_index(&index)? {
        tracing::info!(artifact = %n, "published");
    }
    state.built.insert(sha, out.clone());
    state.save(&run.results)?;
    println!("{}", out.display());
    Ok(out)
}

fn do_replay(run: &Run, range: Option<usize>, resume: bool) -> Result<()> {
    let checkout = run.corpus.open(&run.entry)?;
    let engine = checkout.engine(&run.entry, run.version())?;
    let range = range.unwrap_or(run.entry.history_window);
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
        results_dir: run.results.clone(),
        state_dir: run.results.clone(),
        publish: true,
        resume,
        bazel_version: run.version().unwrap_or_default().to_string(),
        diff_cap: 50,
    };
    let driver = history::BazelDriver {
        checkout: &checkout,
        engine: &engine,
    };
    let result = history::history(&engine, &driver, &commits, &opts)?;
    let path = history::HistoryResult::path_versioned(&run.results, &run.entry.pin, run.version());
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

fn do_change_sets(run: &Run, kinds: &[String], cap: usize, output: DataFormat) -> Result<()> {
    let kinds = if kinds.is_empty() {
        vec![
            changes::ChangeSetKind::Adjacent,
            changes::ChangeSetKind::Pr,
            changes::ChangeSetKind::Stress,
        ]
    } else {
        kinds
            .iter()
            .map(|k| {
                changes::ChangeSetKind::parse(k).ok_or_else(|| {
                    anyhow::anyhow!("unknown change_set kind {k:?}: adjacent, pr, stress")
                })
            })
            .collect::<Result<Vec<_>>>()?
    };
    let checkout = run.corpus.open(&run.entry)?;
    let wants_hubs = kinds.contains(&changes::ChangeSetKind::Stress);
    let hub_packages = if wants_hubs {
        hub_packages(run)?
    } else {
        Vec::new()
    };
    let (paths, globs) = run.entry.invalidator_rules();
    let opts = changes::ChangeSetOptions {
        repo_id: run.entry.id.clone(),
        pin: run.entry.pin.clone(),
        history_window: run.entry.history_window,
        seed: run.seed,
        cap,
        total_cap: Some(cap),
        hub_packages,
        invalidator_paths: paths,
        invalidator_globs: globs,
    };
    let set = changes::select(&checkout.repo, &kinds, &opts)?;
    let path = run.result_path(&format!("change_sets-{}", run.entry.pin), "json");
    set.write(&path)?;
    tracing::info!(change_sets = set.change_sets.len(), path = %path.display(), "selected change_sets");
    match output {
        DataFormat::Json => println!("{}", serde_json::to_string_pretty(&set)?),
        DataFormat::Text => {
            for p in &set.change_sets {
                println!(
                    "{}\t{}\t{}\t{}",
                    p.kind.name(),
                    p.category.as_deref().unwrap_or("-"),
                    p.base,
                    p.head
                );
            }
        }
    }
    Ok(())
}

/// What `corpus compare` was asked to do, defaulted the way `all` wants it.
#[derive(Clone, Debug)]
struct CompareArgs {
    change_sets: Option<PathBuf>,
    kinds: Vec<String>,
    oracle_sample: usize,
    bazel_diff: String,
    tango: bool,
    no_tango: bool,
    tools: Vec<String>,
    cap: usize,
    restart: bool,
}

impl Default for CompareArgs {
    fn default() -> Self {
        Self {
            change_sets: None,
            kinds: Vec::new(),
            oracle_sample: 100,
            bazel_diff: "rust".into(),
            tango: false,
            no_tango: false,
            tools: Vec::new(),
            cap: cambium_bench::DEFAULT_CHANGE_SETS,
            restart: false,
        }
    }
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
        if !self.tools.is_empty() {
            let mut out = self.tools.clone();
            // `--tools` is the list, but `--tango` alongside it still
            // means "and Tango": the flag never silently does nothing.
            if self.tango && !out.iter().any(|p| p == cambium_bench::tools::tango::NAME) {
                out.push(cambium_bench::tools::tango::NAME.to_string());
            }
            return Ok(out);
        }
        let mut out = Vec::new();
        match self.bazel_diff.as_str() {
            "none" | "off" | "" => {}
            other => {
                let imp = cambium_bench::Implementation::parse(other).ok_or_else(|| {
                    anyhow::anyhow!("--bazel-diff {other:?}: expected rust, kotlin, or none")
                })?;
                out.push(imp.name().to_string());
            }
        }
        if !self.no_tango {
            out.push(cambium_bench::tools::tango::NAME.to_string());
        }
        anyhow::ensure!(!out.is_empty(), "no tools selected");
        Ok(out)
    }
}

/// Locates one tool by name, or explains why it is being skipped.
///
/// A missing binary is a warning, not an error: the machine that runs the
/// nightly corpus job may have bazel-diff and no Go toolchain, and a run that
/// refuses to compare anything because one of two optional tools is absent is
/// worse than a run that says so and compares the other.
fn open_tool(name: &str) -> Result<Option<Box<dyn cambium_bench::Tool>>> {
    use cambium_bench::{BazelDiff, Implementation, Tango};
    let found: Result<Box<dyn cambium_bench::Tool>> = match name {
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
            eprintln!("cambium: corpus compare: skipping {name}: {e:#}");
            Ok(None)
        }
    }
}

fn do_compare(run: &Run, args: &CompareArgs) -> Result<u8> {
    let wanted = args.wanted()?;
    let owned: Vec<Box<dyn cambium_bench::Tool>> = wanted
        .iter()
        .map(|n| open_tool(n))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    if owned.is_empty() {
        eprintln!("cambium: corpus compare: no tool is installed; nothing to compare");
        return Ok(crate::EXIT_ERROR);
    }
    let tools: Vec<&dyn cambium_bench::Tool> = owned.iter().map(|p| p.as_ref()).collect();

    let checkout = run.corpus.open(&run.entry)?;
    let set = load_change_sets(run, &checkout, args)?;
    // The quick profile's change set set is already capped at its own total, so the
    // per-run `--cap` (`FULL_PAIR_CAP` by default, meant to bound an EC2 job's
    // tail) must not shave it down further.
    let cap = args.cap;
    let change_sets: Vec<_> = set.change_sets.iter().take(cap.max(1)).cloned().collect();
    anyhow::ensure!(!change_sets.is_empty(), "no change_sets to compare");

    let engine = checkout.engine(&run.entry, run.version())?;
    let opts = cambium_bench::ChangesOptions {
        bazel_version: run.version.clone(),
        oracle_k: args.oracle_sample,
        seed: run.seed,
        resume: !args.restart,
        scratch: run.results.join("scratch"),
        ..cambium_bench::ChangesOptions::new(
            run.entry.id.clone(),
            run.entry.pin.clone(),
            run.results.clone(),
        )
    };
    tracing::info!(
        change_sets = change_sets.len(),
        tools = tools.len(),
        oracle_k = opts.oracle_k,
        tango_change_sets = opts.tango_change_sets.unwrap_or(change_sets.len()),
        "comparing"
    );
    let result =
        cambium_bench::compare_change_sets(&engine, &checkout, &change_sets, &tools, &opts)?;
    print_compare_summary(&result);
    println!("{}", opts.output_path().display());
    Ok(if result.summary.pass {
        crate::EXIT_OK
    } else {
        crate::EXIT_ERROR
    })
}

/// The change sets to compare: an explicit file, else the one `corpus change_sets` wrote
/// for this pin, else a fresh selection with the requested kinds.
fn load_change_sets(
    run: &Run,
    checkout: &cambium_bench::Checkout,
    args: &CompareArgs,
) -> Result<changes::ChangeSetSet> {
    if let Some(p) = &args.change_sets {
        return changes::ChangeSetSet::read(p).with_context(|| format!("reading {}", p.display()));
    }
    let default = run.result_path(&format!("change_sets-{}", run.entry.pin), "json");
    if args.kinds.is_empty() && default.exists() {
        tracing::info!(path = %default.display(), "using the recorded change_sets");
        return changes::ChangeSetSet::read(&default);
    }
    let kinds = parse_kinds(&args.kinds)?;
    let (paths, globs) = run.entry.invalidator_rules();
    let opts = changes::ChangeSetOptions {
        repo_id: run.entry.id.clone(),
        pin: run.entry.pin.clone(),
        history_window: run.entry.history_window,
        seed: run.seed,
        cap: args.cap,
        total_cap: Some(args.cap),
        hub_packages: Vec::new(),
        invalidator_paths: paths,
        invalidator_globs: globs,
    };
    let set = changes::select(&checkout.repo, &kinds, &opts)?;
    set.write(&default)?;
    Ok(set)
}

fn parse_kinds(kinds: &[String]) -> Result<Vec<changes::ChangeSetKind>> {
    if kinds.is_empty() {
        return Ok(vec![changes::ChangeSetKind::Adjacent]);
    }
    kinds
        .iter()
        .map(|k| {
            changes::ChangeSetKind::parse(k).ok_or_else(|| {
                anyhow::anyhow!("unknown change_set kind {k:?}: adjacent, pr, stress")
            })
        })
        .collect()
}

/// The table a person reads before opening the JSON.
fn print_compare_summary(result: &cambium_bench::ChangesResult) {
    let s = &result.summary;
    println!(
        "\n{} {}  change_sets {}  passed {}  global {}  min recall {:.1}%",
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

/// `docs/CORPUS.md`: this repo's section, plus the top-level table rebuilt
/// from every repository that has results.
fn do_report(ctx: &Context, run: &Run, doc: Option<&std::path::Path>) -> Result<u8> {
    // `docs/CORPUS.md` is the published document, and a quick run's numbers are
    // a laptop's slice: they go to a file of their own next to the results they
    // came from, unless the caller names a document explicitly.
    let doc = doc
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| ctx.root.join("docs/CORPUS.md"));

    // Every repository at once, not just this one: a per-repo nightly job
    // rewrites the top table and the synthetic headline wholesale, and must
    // not drop the other repos' rows on the way. Within
    // one profile, though: quick and full numbers never share a table.
    let parent = run.results_root.clone();
    let repos = collect_repos(&run.manifest, &parent, &run.corpus)?;
    let mine = repos
        .iter()
        .find(|r| r.id == run.entry.id)
        .cloned()
        .unwrap_or_else(|| cambium_bench::SubjectResults::new(&run.entry.id));
    if mine.is_empty() {
        eprintln!(
            "cambium: corpus report: no history-*.json or compare-*.json in {}",
            run.results.display()
        );
        return Ok(crate::EXIT_ERROR);
    }

    let bench = read_bench_legacy(&ctx.root.join("docs/benchmarks/sweep-legacy.json"))?;
    cambium_bench::update_doc(
        &doc,
        &run.entry.id,
        &cambium_bench::render_subject_section(&mine, None),
    )?;
    cambium_bench::update_doc(&doc, "top", &cambium_bench::render_top_table(&repos))?;
    cambium_bench::update_doc(
        &doc,
        "bench",
        &cambium_bench::render_bench_section(&bench, Some("bench-legacy.svg")),
    )?;
    tracing::info!(doc = %doc.display(), repos = repos.len(), "report written");
    println!("{}", doc.display());
    Ok(crate::EXIT_OK)
}

/// Results for every manifest entry that has a results directory, in manifest
/// order, plus any stray directory the manifest no longer names (a renamed or
/// removed entry still has numbers worth showing rather than silently losing).
fn collect_repos(
    manifest: &Manifest,
    parent: &std::path::Path,
    corpus: &SubjectsDir,
) -> Result<Vec<cambium_bench::SubjectResults>> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for entry in &manifest.repos {
        let dir = parent.join(&entry.id);
        seen.insert(entry.id.clone());
        if !dir.is_dir() {
            continue;
        }
        let (mut history, compares) = read_results(&dir)?;
        // A history written before `bazel_version_used` existed, with no
        // version override, ran whatever the worktree's `.bazelversion`
        // asked bazelisk for; that file is the honest record.
        if let Some(r) = history.as_mut() {
            if r.bazel_version_used.is_empty() && r.bazel_version.is_empty() {
                let path = corpus.worktree(&entry.id).join(".bazelversion");
                if let Ok(v) = std::fs::read_to_string(path) {
                    r.bazel_version_used = v.trim().to_string();
                }
            }
        }
        let r = cambium_bench::SubjectResults::new(&entry.id)
            .with_replay(history)
            .with_compares(compares);
        if !r.is_empty() {
            out.push(r);
        }
    }
    let mut strays: Vec<PathBuf> = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    strays.sort();
    for dir in strays {
        let Some(id) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if seen.contains(id) {
            continue;
        }
        let (history, compares) = read_results(&dir)?;
        let r = cambium_bench::SubjectResults::new(id)
            .with_replay(history)
            .with_compares(compares);
        if !r.is_empty() {
            out.push(r);
        }
    }
    Ok(out)
}

/// The saved `bench sweep --stage legacy_bazel_diff` run, as report rows.
///
/// Missing or unparseable is not an error: the synthetic section then says the
/// sweep has not been recorded, which is true and better than failing a corpus
/// report over a benchmark file.
fn read_bench_legacy(path: &std::path::Path) -> Result<Vec<cambium_bench::BenchLegacy>> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "ignoring an unreadable bench file");
            return Ok(Vec::new());
        }
    };
    let mut rows = Vec::new();
    for r in v
        .get("results")
        .and_then(|r| r.as_array())
        .into_iter()
        .flatten()
    {
        if r.get("stage").and_then(|s| s.as_str()) != Some("legacy_bazel_diff") {
            continue;
        }
        let n = |k: &str| -> u64 {
            r.get("extra")
                .and_then(|e| e.get(k))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                .round() as u64
        };
        let top =
            |k: &str| -> u64 { r.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0).round() as u64 };
        let targets = match n("targets") {
            0 => top("param"),
            t => t,
        };
        let cambium_only = n("cambium_only");
        let legacy_only = n("bazel_diff_only");
        rows.push(cambium_bench::BenchLegacy {
            preset: r
                .get("preset")
                .and_then(|p| p.as_str())
                .unwrap_or_default()
                .to_string(),
            targets,
            legacy_cold_ms: n("bazel_diff_cold_ms"),
            legacy_warm_ms: n("bazel_diff_warm_ms"),
            cambium_cold_ms: 0,
            cambium_warm_ms: match n("cambium_ms") {
                0 => top("median_ms"),
                v => v,
            },
            cambium_p90_ms: match n("cambium_p90_ms") {
                0 => top("p90_ms"),
                v => v,
            },
            agreed_labels: n("agreed"),
            cambium_only,
            legacy_only,
            agreed: cambium_only == 0 && legacy_only == 0,
        });
    }
    Ok(rows)
}

/// Every `history-*.json` and `compare-*.json` in one results directory.
///
/// More than one history can be present (a repository whose `bazel_version` was
/// changed leaves the older file behind); the newest by name wins for the
/// section's history tables, while every compare is rendered.
fn read_results(
    dir: &std::path::Path,
) -> Result<(
    Option<cambium_bench::HistoryResult>,
    Vec<cambium_bench::ChangesResult>,
)> {
    let mut replays: Vec<PathBuf> = Vec::new();
    let mut compares: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".json") {
            continue;
        }
        if name.starts_with("history-") {
            replays.push(path);
        } else if name.starts_with("compare-") {
            compares.push(path);
        }
    }
    replays.sort();
    compares.sort();
    let history = replays
        .last()
        .map(|p| {
            cambium_bench::HistoryResult::read(p)
                .with_context(|| format!("reading {}", p.display()))
        })
        .transpose()?;
    let compares = compares
        .iter()
        .map(|p| {
            cambium_bench::ChangesResult::read(p)
                .with_context(|| format!("reading {}", p.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((history, compares))
}

/// The packages of the built index's busiest targets: an edit anywhere in one
/// of these reaches the largest share of the graph, which is what the stress
/// change sets are for.
fn hub_packages(run: &Run) -> Result<Vec<String>> {
    let state = run.state()?;
    let path = state
        .built
        .get(&run.entry.pin)
        .cloned()
        .unwrap_or_else(|| run.index_path(&run.entry.pin));
    anyhow::ensure!(
        path.exists(),
        "stress change_sets need the index at the pin; run `cambi corpus build {}` first",
        run.entry.id
    );
    let file = cambium_index::IndexFile::open(&path)?;
    let data = file.deserialize()?;
    let stats: IndexStats = IndexQuery::new(data.view()).stats(0);
    let mut pkgs: Vec<String> = stats
        .in_degree
        .top
        .iter()
        .map(|h| package_of(&h.label))
        .collect();
    pkgs.sort();
    pkgs.dedup();
    tracing::info!(hubs = pkgs.len(), "hub packages from the index");
    Ok(pkgs)
}

/// `//src/main/java/foo:bar` -> `src/main/java/foo`.
fn package_of(label: &str) -> String {
    let l = label.strip_prefix("//").unwrap_or(label);
    match l.split_once(':') {
        Some((pkg, _)) => pkg.to_string(),
        None => l.to_string(),
    }
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

    #[test]
    fn result_names_carry_the_bazel_version() {
        assert_eq!(suffixed("index-abc", None, "bin"), "index-abc.bin");
        assert_eq!(suffixed("index-abc", Some(""), "bin"), "index-abc.bin");
        assert_eq!(
            suffixed("history-abc", Some("7.x"), "json"),
            "history-abc-7.x.json"
        );
        assert_eq!(
            suffixed("index-abc", Some("last/green"), "bin"),
            "index-abc-last_green.bin"
        );
    }

    #[test]
    fn labels_reduce_to_their_package() {
        assert_eq!(package_of("//src/main/java/foo:bar"), "src/main/java/foo");
        assert_eq!(package_of("//:root"), "");
        assert_eq!(package_of("//a/b"), "a/b");
    }
}
