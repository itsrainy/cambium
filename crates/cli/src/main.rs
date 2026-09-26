//! `cambi` — incremental changed-target computation for Bazel monorepos.
//!
//! Eight verbs, and nothing else is in this binary: `init`, `status`,
//! `doctor`, `affected`, `explain`, `index`, `inspect`, `config`. The surface
//! they make — what each one is for, the configuration, the output contract,
//! the diagnostics format and the deprecated spellings still accepted — is
//! `docs/CLI.md`, and `crates/cli/tests/cli_contract.rs` pins it. The
//! benchmark harness is a separate binary, `cambium-bench`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

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
    // at build time by this crate's build.rs, which says where it comes from.
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

    /// Colour on stderr. `auto` means: a TTY, `$NO_COLOR` unset,
    /// `$CLICOLOR` not 0 and `$TERM` not dumb; `$CLICOLOR_FORCE` colours a
    /// pipe.
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
    /// Why a target is affected, or why the run went global.
    Explain(cmd::explain::Args),
    /// What cambium costs and saves here: graph size, cold build, and the
    /// last N commits' incremental times and rebuilds.
    Profile(cmd::profile::Args),
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
    /// A `set(...)` expression for `bazel query` / `bazel cquery`; `bazel
    /// build` and `bazel test` reject it, and take `labels` instead.
    /// `affected` only; anywhere else it is a usage error.
    Query,
    /// One label per line and nothing else, for `bazel test
    /// --target_pattern_file`: empty on an empty answer, `//...` on a global
    /// one. `affected` only; anywhere else it is a usage error.
    Labels,
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
    /// prints nothing. `query` and `labels` have no meaning here (there is no
    /// Bazel expression or label list for a byte count), so they are a usage
    /// error rather than a silent fallback to something else.
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
            Format::Labels => Err(diag::UsageError::raised(format!(
                "--format labels is only meaningful for `cambi affected`; {verb} has no \
                 target list"
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

impl From<ColorChoice> for cambium_ui::ColorChoice {
    fn from(c: ColorChoice) -> Self {
        match c {
            ColorChoice::Auto => cambium_ui::ColorChoice::Auto,
            ColorChoice::Always => cambium_ui::ColorChoice::Always,
            ColorChoice::Never => cambium_ui::ColorChoice::Never,
        }
    }
}

fn main() -> ExitCode {
    quiet_on_a_closed_stdout();
    // The deprecated spellings are rewritten into the current ones before
    // clap sees them (see `compat`); the diagnostics they raised are printed
    // below, once `--quiet` and `--format` are known.
    let compat::Rewritten { argv, notes } = compat::rewrite(std::env::args().collect());
    // Through the matches rather than `parse_from`, so that where each value
    // came from is still known: `--cache-dir` and `$CAMBIUM_CACHE_DIR` reach
    // the struct as one `Option`, and `cambi config explain` has to say
    // which one set it.
    let matches = match Cli::command().try_get_matches_from(argv) {
        Ok(m) => m,
        Err(e) => return clap_exit(&e),
    };
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => return clap_exit(&e),
    };
    let overrides = overrides(&cli, &matches);

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
    // One colour rule for the diagnostics and the narration (the ui crate's):
    // a diagnostic that ignored CLICOLOR or TERM=dumb would paint escapes
    // into a log the narration beside it kept plain.
    let render = diag::Renderer {
        format: cli.format,
        quiet: cli.quiet,
        color: cambium_ui::OutputMode::stderr_colour(cli.color.into()),
    };
    let ui = cambium_ui::OutputMode::detect(
        cli.quiet,
        cli.format == Format::Jsonl,
        cli.color.into(),
    );
    // After the output mode, because the log lines go to the same stderr and
    // colour by the same rule: escape codes on a terminal, plain bytes in a
    // pipe or a CI log.
    init_logging(&cli.log, ui.colour);
    let ctx = cmd::Context {
        root,
        config: cli.config.clone(),
        overrides,
        format: cli.format,
        render,
        ui,
    };

    for d in &notes {
        render.warn(d);
    }

    let code = match cli.command.unwrap_or(Command::Status) {
        Command::Init(args) => cmd::init::run(&ctx, args),
        Command::Status => cmd::status::run(&ctx),
        Command::Doctor => cmd::doctor::run(&ctx),
        Command::Affected(args) => cmd::affected::run(&ctx, args),
        Command::Explain(args) => cmd::explain::run(&ctx, args),
        Command::Profile(args) => cmd::profile::run(&ctx, args),
        Command::Index(args) => cmd::index::run(&ctx, args),
        Command::Inspect(args) => cmd::inspect::run(&ctx, args),
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

/// Where clap's own answer goes, and the code it exits with.
///
/// `--help` and `--version` are answers: stdout, exit 0. Anything else clap
/// stops on (an unknown flag or verb, a missing value, a verb with no
/// subcommand) is a command line that does not parse, and that is an error,
/// exit 1. Clap's own default is 2, which is this binary's "everything is
/// affected": a CI script that tests `//...` on exit 2 would test everything
/// on a typo. Clap's message and its `--help` hint are printed unchanged.
fn clap_exit(e: &clap::Error) -> ExitCode {
    let _ = e.print();
    ExitCode::from(clap_exit_code(e))
}

fn clap_exit_code(e: &clap::Error) -> u8 {
    if e.use_stderr() {
        EXIT_ERROR
    } else {
        EXIT_OK
    }
}

/// The values the command line and the environment set, each with the
/// spelling to attribute it to. Clap knows which of a flag and its `env` fell
/// in, and that is the only place the question can be answered without
/// consulting the environment a second time.
fn overrides(cli: &Cli, matches: &clap::ArgMatches) -> cambium_engine::config::Overrides {
    use cambium_engine::config::Origin;
    use clap::parser::ValueSource;
    cambium_engine::config::Overrides {
        // `--bazel-binary` has no environment spelling: `$BAZEL_REAL` and
        // `$BAZEL` are read by the Bazel handle itself, below the config.
        bazel_binary: cli.bazel_binary.clone().map(|b| {
            (
                b,
                Origin::Flag {
                    name: "--bazel-binary".into(),
                },
            )
        }),
        cache_dir: cli.cache_dir.clone().map(|d| {
            let origin = match matches.value_source("cache_dir") {
                Some(ValueSource::EnvVariable) => Origin::Env {
                    name: "CAMBIUM_CACHE_DIR".into(),
                },
                _ => Origin::Flag {
                    name: "--cache-dir".into(),
                },
            };
            (d, origin)
        }),
    }
}

/// A reader that closed stdout early is not a failure of this run.
///
/// `cambi affected --format json | head` stops reading after ten lines, and
/// the next `println!` fails with `EPIPE` (Rust ignores `SIGPIPE`, so the
/// write returns an error rather than killing the process), which `println!`
/// turns into a panic: exit 101 and a panic message on stderr. Every stdout
/// write in this binary is a `println!` or `print!`, so the panic hook is the
/// one place to catch it.
///
/// The exit is 0, not 1. The answer was computed; the reader took what it
/// wanted and left. A 1 would fail a `set -o pipefail` pipeline whose own
/// consumer succeeded, and when the consumer did fail, its status is the one
/// the pipeline reports. Any other stdout failure (a full disk behind a
/// redirect) is still the panic it was.
fn quiet_on_a_closed_stdout() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if info.payload_as_str().is_some_and(is_closed_stdout) {
            std::process::exit(EXIT_OK.into());
        }
        default(info);
    }));
}

/// `println!`'s panic message for a write to a pipe with no reader:
/// `failed printing to stdout: Broken pipe (os error 32)`.
fn is_closed_stdout(panic: &str) -> bool {
    panic.starts_with("failed printing to stdout")
        && panic.to_ascii_lowercase().contains("broken pipe")
}

fn init_logging(level: &str, colour: bool) {
    use tracing_subscriber::{fmt, EnvFilter};
    // `cli.log` already carries $CAMBIUM_LOG's value via clap's `env =
    // "CAMBIUM_LOG"` (an explicit `--log` still wins over it there), so this
    // must not consult the environment a second time — doing so let a stale
    // $CAMBIUM_LOG silently override an explicit `--log`.
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(colour)
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
    fn only_a_reader_that_left_is_a_quiet_exit() {
        assert!(is_closed_stdout(
            "failed printing to stdout: Broken pipe (os error 32)"
        ));
        assert!(
            !is_closed_stdout("failed printing to stdout: No space left on device (os error 28)"),
            "a full disk behind a redirect is a real failure"
        );
        assert!(!is_closed_stdout(
            "failed printing to stderr: Broken pipe (os error 32)"
        ));
    }

    #[test]
    fn a_command_line_clap_rejects_is_an_error_and_help_is_an_answer() {
        let code = |argv: &[&str]| match Cli::command().try_get_matches_from(argv) {
            Ok(_) => None,
            Err(e) => Some(clap_exit_code(&e)),
        };
        assert_eq!(
            code(&["cambi", "affected", "--not-a-flag"]),
            Some(EXIT_ERROR)
        );
        assert_eq!(
            code(&["cambi", "affected", "--max-depth"]),
            Some(EXIT_ERROR)
        );
        assert_eq!(code(&["cambi", "no-such-verb"]), Some(EXIT_ERROR));
        assert_eq!(code(&["cambi", "index"]), Some(EXIT_ERROR));
        assert_eq!(code(&["cambi", "--help"]), Some(EXIT_OK));
        assert_eq!(code(&["cambi", "affected", "--help"]), Some(EXIT_OK));
        assert_eq!(code(&["cambi", "--version"]), Some(EXIT_OK));
        assert_ne!(
            Some(EXIT_GLOBAL),
            code(&["cambi", "--format", "nope"]),
            "a typo must never read as everything affected"
        );
    }

    #[test]
    fn the_cli_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
