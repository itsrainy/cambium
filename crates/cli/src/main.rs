//! `cambium` — incremental changed-target computation for Bazel monorepos.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

mod cmd;

/// Exit codes (spec 10).
pub const EXIT_OK: u8 = 0;
pub const EXIT_ERROR: u8 = 1;
pub const EXIT_GLOBAL: u8 = 2;
pub const EXIT_BAZEL: u8 = 3;
pub const EXIT_NO_BASELINE: u8 = 4;

#[derive(Parser)]
#[command(
    name = "cambium",
    version,
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

    /// Log level: error, warn, info, debug, trace.
    #[arg(long, global = true, default_value = "warn", env = "CAMBIUM_LOG")]
    log: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Report the targets affected by the changes between two revisions.
    Affected(cmd::affected::Args),
    /// Manage baseline indexes.
    #[command(subcommand)]
    Baseline(cmd::baseline::Args),
    /// Inspect and check index files.
    #[command(subcommand)]
    Index(cmd::index::Args),
    /// Print the effective configuration and its hash.
    Config,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// One label per line.
    Text,
    /// The full report, including timings and stats.
    Json,
    /// A `set(...)` expression for `bazel build` / `bazel test`.
    Query,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(&cli.log);

    let root = cli
        .workspace
        .clone()
        .unwrap_or_else(|| std::env::current_dir().expect("current directory is readable"));
    let ctx = cmd::Context {
        root,
        config: cli.config.clone(),
        bazel_binary: cli.bazel_binary.clone(),
    };

    let code = match cli.command {
        Command::Affected(args) => cmd::affected::run(&ctx, args),
        Command::Baseline(args) => cmd::baseline::run(&ctx, args),
        Command::Index(args) => cmd::index::run(&ctx, args),
        Command::Config => cmd::config_show(&ctx),
    };

    match code {
        Ok(c) => ExitCode::from(c),
        Err(e) => {
            eprintln!("cambium: {e:#}");
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
    let filter = EnvFilter::try_from_env("CAMBIUM_LOG")
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}
