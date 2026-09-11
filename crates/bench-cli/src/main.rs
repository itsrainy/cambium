//! `cambium-bench` — the benchmark harness (docs/METHODOLOGY.md).
//!
//! Not the product. `cambi` answers the one question the contract is about;
//! this binary measures it, and it does so the way a user would — by running
//! `cambi` as a child process and reading the documents it prints. What the
//! harness links the engine for is the *offline* half: the report types, the
//! replay of engine stages against a stored index, and the in-process stage
//! micro-benchmarks. See docs/CLI.md, "The benchmark tools".

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use cambium_bench::session::{Session, SessionOptions};
use clap::{Parser, ValueEnum};

mod bench;
mod launcher;
mod replay;
mod synth;
mod watch;

pub const EXIT_OK: u8 = 0;
pub const EXIT_ERROR: u8 = 1;

pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("CAMBIUM_GIT_SHA"),
    ")"
);

#[derive(Parser)]
#[command(
    name = "cambium-bench",
    version = VERSION,
    about = "The cambium benchmark: subjects, workloads, oracles, replay and the report",
    long_about = "Runs cambium against real and synthetic Bazel repositories and against the \
other tools that answer the same question. The product binary is `cambi`, which this drives as \
a child process; nothing here is part of cambium's command-line contract."
)]
struct Cli {
    /// Directory to run from (defaults to the current directory). The
    /// manifest is discovered upward from it.
    #[arg(long, short = 'C', global = true)]
    workspace: Option<PathBuf>,

    /// The `cambi` binary the harness measures. Without it, `cambi` on
    /// `PATH`.
    #[arg(long, global = true, value_name = "PATH", env = "CAMBI")]
    cambi: Option<PathBuf>,

    /// The bazel-diff Rust binary; default: under --bazel-diff-dir.
    #[arg(long, global = true, value_name = "PATH", env = cambium_bench::tools::bazeldiff::BIN_ENV)]
    bazel_diff: Option<PathBuf>,

    /// The bazel-diff deploy jar; default: under --bazel-diff-dir.
    #[arg(long, global = true, value_name = "PATH", env = cambium_bench::tools::bazeldiff::JAR_ENV)]
    bazel_diff_jar: Option<PathBuf>,

    /// Where tools/bazel-diff/fetch.sh installed (default:
    /// ~/.cache/cambium/bazel-diff).
    #[arg(long, global = true, value_name = "DIR", env = cambium_bench::tools::bazeldiff::DIR_ENV)]
    bazel_diff_dir: Option<PathBuf>,

    /// The tango-cli binary; default: under --tango-dir.
    #[arg(long, global = true, value_name = "PATH", env = cambium_bench::tools::tango::BIN_ENV)]
    tango: Option<PathBuf>,

    /// Where tools/tango/fetch.sh installed (default: ~/.cache/cambium/tango).
    #[arg(long, global = true, value_name = "DIR", env = cambium_bench::tools::tango::DIR_ENV)]
    tango_dir: Option<PathBuf>,

    /// Bazel binary to use; overrides the subject's own.
    #[arg(long, global = true)]
    bazel_binary: Option<String>,

    /// Log level: error, warn, info, debug, trace.
    #[arg(long, global = true, default_value = "warn", env = "CAMBIUM_LOG")]
    log: String,

    /// How to print the answer.
    #[arg(
        long,
        value_enum,
        global = true,
        default_value_t = Format::Human,
        value_name = "FORMAT"
    )]
    format: Format,

    /// Say nothing on stderr when the run succeeds.
    #[arg(long, short = 'q', global = true)]
    quiet: bool,

    /// No verb at all, on a terminal, is the launcher: a screen that composes
    /// a run and then watches it.
    #[command(subcommand)]
    command: Option<bench::Args>,
}

/// The harness follows `cambi`'s output rules because they are good rules, not
/// because it is part of the contract.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Format {
    #[value(alias = "text")]
    Human,
    Json,
    Jsonl,
    Null,
}

/// What a command that answers with a document distinguishes, once `--format`
/// has been resolved. `None` is `--format null`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DataFormat {
    Text,
    Json,
    Jsonl,
}

impl Format {
    pub fn data(self) -> Option<DataFormat> {
        match self {
            Format::Human => Some(DataFormat::Text),
            Format::Json => Some(DataFormat::Json),
            Format::Jsonl => Some(DataFormat::Jsonl),
            Format::Null => None,
        }
    }
}

/// The globals, as parsed. A verb opens a [`Session`] from them plus its
/// own shared flags; the session is what the library works against.
pub struct Context {
    /// Where the manifest is discovered from.
    pub root: PathBuf,
    /// An explicit `--bazel-binary`, overriding the subject's.
    pub bazel_binary: Option<String>,
    pub format: Format,
    pub quiet: bool,
    /// `--cambi`, if it was given.
    pub cambi: Option<PathBuf>,
    /// Where the foil tools are, from the flags.
    pub tools: cambium_bench::tools::ToolLocations,
}

impl Context {
    /// The session a verb runs in: the manifest found, the results directory
    /// made absolute, `cambi` located on first use.
    pub fn session(&self, common: &bench::Common) -> anyhow::Result<Session> {
        Session::open(SessionOptions {
            root: self.root.clone(),
            manifest: common.manifest.clone(),
            results_dir: common.results_dir.clone(),
            subjects_dir: common.subjects_dir.clone(),
            bazel_binary: self.bazel_binary.clone(),
            bazel_version: common.bazel_version.clone(),
            seed: common.seed,
            quick: common.quick,
            cambi: self.cambi.clone(),
        })
    }
}

/// One `jsonl` result line: the document, with `"event": "result"` in front of
/// its own fields. The same shape `cambi` writes.
pub fn result_event<T: serde::Serialize>(value: &T) -> anyhow::Result<String> {
    let mut doc = serde_json::json!({"event": "result"});
    let map = doc.as_object_mut().expect("an object");
    match serde_json::to_value(value)? {
        serde_json::Value::Object(fields) => map.extend(fields),
        other => {
            map.insert("result".into(), other);
        }
    }
    Ok(serde_json::to_string(&doc)?)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(&cli.log);
    let root = match cli.workspace.clone() {
        Some(w) => w,
        None => match std::env::current_dir() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("cambium-bench: could not read the current directory: {e}");
                return ExitCode::from(EXIT_ERROR);
            }
        },
    };
    let ctx = Context {
        root,
        bazel_binary: cli.bazel_binary.clone(),
        format: cli.format,
        quiet: cli.quiet,
        cambi: cli.cambi.clone(),
        tools: cambium_bench::tools::ToolLocations {
            bazel_diff: cli.bazel_diff.clone(),
            bazel_diff_jar: cli.bazel_diff_jar.clone(),
            bazel_diff_dir: cli.bazel_diff_dir.clone(),
            tango: cli.tango.clone(),
            tango_dir: cli.tango_dir.clone(),
        },
    };
    let outcome = match cli.command {
        Some(command) => bench::run(&ctx, command),
        // No verb. On a terminal that is the launcher — the first thing
        // somebody types is the binary's name, and answering it with a usage
        // screen spends the one command they ran on nothing. In a pipe there
        // is no screen to draw, so it is the usage error it has always been.
        None if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => {
            launcher::run(&ctx)
        }
        None => {
            use clap::CommandFactory;
            let _ = Cli::command().print_help();
            println!();
            return ExitCode::from(EXIT_ERROR);
        }
    };
    match outcome {
        Ok(c) => ExitCode::from(c),
        Err(e) => {
            eprintln!("cambium-bench: {e:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn init_logging(level: &str) {
    use tracing_subscriber::{fmt, EnvFilter};
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
    fn the_cli_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    /// The verbs that left `cambi`, and nothing of the product's surface.
    #[test]
    fn the_verbs_are_the_harness_verbs() {
        use clap::CommandFactory;
        let cmd = Cli::command();
        let names: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        for v in [
            "subjects",
            "history",
            "changes",
            "stages",
            "sweep",
            "regress",
            "replay",
            "labels",
            "report",
            "synth",
            "watch",
            "run",
            "pull-results",
        ] {
            assert!(names.contains(&v), "{v} is missing from {names:?}");
        }
        for v in [
            "affected", "index", "inspect", "status", "doctor", "init", "config",
        ] {
            assert!(
                !names.contains(&v),
                "{v} belongs to cambi, not here: {names:?}"
            );
        }
    }
}
