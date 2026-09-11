//! `cambium` — incremental changed-target computation for Bazel monorepos.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

mod cmd;
mod compat;
mod diag;

/// Exit codes (spec 10).
///
/// Part of the versioned contract described in the README alongside the JSON
/// field names: callers branch on these, so a value never changes meaning
/// without a `SCHEMA_VERSION` bump.
pub const EXIT_OK: u8 = 0;
pub const EXIT_ERROR: u8 = 1;
pub const EXIT_GLOBAL: u8 = 2;
pub const EXIT_BAZEL: u8 = 3;
pub const EXIT_NO_BASELINE: u8 = 4;
/// The store could not be reached — as against holding no index for this
/// commit, which is [`EXIT_NO_BASELINE`]. A CI job can retry this one.
pub const EXIT_STORE: u8 = 5;

const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("CAMBIUM_GIT_SHA"),
    ")"
);

#[derive(Parser)]
#[command(
    name = "cambi",
    // `cambi 0.1.0 (7f2c666)`: the commit the binary was built from, so a
    // driver can refuse to run a tree the binary is not. The sha is stamped
    // at build time by the bench crate's build.rs.
    version = VERSION,
    about = "Incremental changed-target computation for large Bazel monorepos",
    long_about = "Computes the set of Bazel targets whose outputs could differ between two \
commits, at a cost proportional to the size of the change rather than the size of the \
repository. Over-approximates; never under-approximates."
)]
struct Cli {
    /// Directory inside the Bazel workspace (defaults to the current directory).
    #[arg(long, short = 'C', global = true)]
    workspace: Option<PathBuf>,

    /// Path to cambium.toml (defaults to <workspace>/cambium.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Bazel binary to use; overrides config and $BAZEL_REAL.
    #[arg(long, global = true)]
    bazel_binary: Option<String>,

    /// Where fetched indexes and per-revision worktrees are kept; overrides
    /// `baseline.cache_dir` (default: <tmp>/cambium-cache).
    #[arg(long, global = true, env = "CAMBIUM_CACHE_DIR")]
    cache_dir: Option<PathBuf>,

    /// Log level: error, warn, info, debug, trace.
    #[arg(long, global = true, default_value = "warn", env = "CAMBIUM_LOG")]
    log: String,

    /// How to print the answer. `--output` is the old spelling and still
    /// works; so does `text` for `human`.
    #[arg(
        long,
        value_enum,
        global = true,
        default_value_t = Format::Human,
        alias = "output",
        value_name = "FORMAT"
    )]
    format: Format,

    /// Say nothing on stderr when the run succeeds.
    #[arg(long, short = 'q', global = true)]
    quiet: bool,

    /// Colour on stderr. `auto` means: a TTY, and `$NO_COLOR` unset.
    #[arg(long, value_enum, global = true, default_value_t = ColorChoice::Auto)]
    color: ColorChoice,

    /// No verb at all is `cambi status`: the first thing someone types is the
    /// question "where am I", and answering it with a usage screen wastes the
    /// one command they already ran.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Write cambium.toml, asking only what cannot be detected.
    Init(cmd::init::Args),
    /// Where this checkout is and what index a run would load.
    Status,
    /// Check that a run can work here, and say what to fix if it cannot.
    Doctor,
    /// Report the targets affected by the changes between two revisions.
    Affected(cmd::affected::Args),
    /// Build, publish and prune the stored indexes.
    #[command(subcommand)]
    Index(cmd::index::Args),
    /// Plumbing: graph questions against one index file, for checking an
    /// answer `explain` gave.
    #[command(subcommand)]
    Inspect(cmd::inspect::Args),
    /// Show the effective configuration and its hash.
    #[command(subcommand)]
    Config(cmd::ConfigArgs),
    /// Benchmark cambium against real and synthetic subjects.
    #[command(subcommand)]
    Bench(cmd::bench::Args),
    /// Generate synthetic Bazel workspaces.
    #[command(subcommand)]
    Synth(cmd::synth::Args),
}

/// The one output flag, declared on `cambi` itself (`docs/CLI.md`, "The
/// output contract").
///
/// `human` is the default whether or not stdout is a TTY: it is what `text`
/// was, and a script that never passed `--output` must keep getting what it
/// got. `--output` stays as a hidden alias of `--format`, and `text` as a
/// hidden alias of `human`, so every existing invocation and every script in
/// `tools/` keeps working unchanged.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Lines for a person. The old `text`, which is still accepted.
    #[value(alias = "text")]
    Human,
    /// One JSON document carrying `schema`.
    Json,
    /// One JSON event per line, so a watcher can follow a long run.
    Jsonl,
    /// A `set(...)` expression for `bazel build` / `bazel test`. `affected`
    /// only; anywhere else it is a usage error.
    Query,
    /// Nothing on stdout; the exit code is the whole answer.
    Null,
}

/// What the data-returning queries distinguish internally, once `--format` has
/// been resolved for them. `Null` is the absence of this, not a variant.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DataFormat {
    /// Human-readable lines.
    Text,
    /// A JSON document carrying the schema version.
    Json,
    /// That document as one line, tagged as a result event.
    Jsonl,
}

impl Format {
    /// This format for a command that answers with a document rather than with
    /// a set of targets.
    ///
    /// `None` is `--format null`: the command runs and sets the exit code and
    /// prints nothing. `query` has no meaning here — there is no Bazel
    /// expression for a byte count — so it is a usage error rather than a
    /// silent fallback to something else.
    pub fn data(self, verb: &str) -> anyhow::Result<Option<DataFormat>> {
        match self {
            Format::Human => Ok(Some(DataFormat::Text)),
            Format::Json => Ok(Some(DataFormat::Json)),
            Format::Jsonl => Ok(Some(DataFormat::Jsonl)),
            Format::Null => Ok(None),
            Format::Query => Err(diag::UsageError::raised(format!(
                "--format query is only meaningful for `cambi affected`; {verb} has no Bazel \
                 expression form"
            ))),
        }
    }
}

/// `--color`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    /// Colour when stderr is a TTY and `$NO_COLOR` is unset.
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    /// Whether to colour stderr. `$NO_COLOR` wins over `auto` and loses to an
    /// explicit `--color always`, which is what a caller asking for colour in
    /// a pipe means.
    fn resolve(self) -> bool {
        use std::io::IsTerminal;
        match self {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => {
                std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
            }
        }
    }
}

fn main() -> ExitCode {
    // The deprecated spellings are rewritten into the current ones before
    // clap sees them (see `compat`); the diagnostics they raised are printed
    // below, once `--quiet` and `--format` are known.
    let compat::Rewritten { argv, notes } = compat::rewrite(std::env::args().collect());
    let cli = Cli::parse_from(argv);
    init_logging(&cli.log);

    let root = match cli.workspace.clone() {
        Some(w) => w,
        None => match std::env::current_dir() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("cambium: could not read the current directory: {e}");
                return ExitCode::from(EXIT_ERROR);
            }
        },
    };
    let render = diag::Renderer {
        format: cli.format,
        quiet: cli.quiet,
        color: cli.color.resolve(),
    };
    let ctx = cmd::Context {
        root,
        config: cli.config.clone(),
        bazel_binary: cli.bazel_binary.clone(),
        cache_dir: cli.cache_dir.clone(),
        format: cli.format,
        render,
    };

    for d in &notes {
        render.warn(d);
    }

    let code = match cli.command.unwrap_or(Command::Status) {
        Command::Init(args) => cmd::init::run(&ctx, args),
        Command::Status => cmd::status::run(&ctx),
        Command::Doctor => cmd::doctor::run(&ctx),
        Command::Affected(args) => cmd::affected::run(&ctx, args),
        Command::Index(args) => cmd::index::run(&ctx, args),
        Command::Inspect(args) => cmd::inspect::run(&ctx, args),
        Command::Bench(args) => cmd::bench::run(&ctx, args),
        Command::Synth(args) => cmd::synth::run(&ctx, args),
        Command::Config(args) => cmd::config(&ctx, args),
    };

    match code {
        Ok(c) => ExitCode::from(c),
        Err(e) => {
            // The same shape a warning gets: `error[CAMBI-E006]: ...` with a
            // `help:` line, or one JSON object under --format json/jsonl.
            render.error(&diag::of_error(&e));
            let code = e
                .downcast_ref::<cambium_engine::EngineError>()
                .map(|e| e.exit_code() as u8)
                .unwrap_or(EXIT_ERROR);
            ExitCode::from(code)
        }
    }
}

fn init_logging(level: &str) {
    use tracing_subscriber::{fmt, EnvFilter};
    // `cli.log` already carries $CAMBIUM_LOG's value via clap's `env =
    // "CAMBIUM_LOG"` (an explicit `--log` still wins over it there), so this
    // must not consult the environment a second time — doing so let a stale
    // $CAMBIUM_LOG silently override an explicit `--log`.
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_pinned() {
        // Changing any of these is a contract break: bump SCHEMA_VERSION and
        // update the JSON contract table in the README.
        assert_eq!(EXIT_OK, 0);
        assert_eq!(EXIT_ERROR, 1);
        assert_eq!(EXIT_GLOBAL, 2);
        assert_eq!(EXIT_BAZEL, 3);
        assert_eq!(EXIT_NO_BASELINE, 4);
        assert_eq!(EXIT_STORE, 5);
    }

    #[test]
    fn engine_errors_map_onto_the_documented_codes() {
        let map = |e: cambium_engine::EngineError| e.exit_code() as u8;
        assert_eq!(
            map(cambium_engine::EngineError::BaselineUnavailable { base: "x".into() }),
            EXIT_NO_BASELINE
        );
        assert_eq!(
            map(cambium_engine::EngineError::Store(
                cambium_engine::store::StoreError::Backend {
                    op: "fetch",
                    name: "indexes/x".into(),
                    detail: "exit Some(1)".into(),
                }
            )),
            EXIT_STORE,
            "a store that cannot be reached is retryable and has its own code"
        );
        assert_eq!(
            map(cambium_engine::EngineError::Store(
                cambium_engine::store::StoreError::NotConfigured
            )),
            EXIT_ERROR,
            "a store that is not configured is a configuration error"
        );
        assert_eq!(
            map(cambium_engine::EngineError::BazelQuery {
                what: "x".into(),
                code: Some(1),
                stderr: String::new(),
            }),
            EXIT_BAZEL
        );
    }

    #[test]
    fn the_cli_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
