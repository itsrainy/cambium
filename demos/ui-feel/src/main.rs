//! A bench of terminal output shapes for two cambium commands, so the look
//! and feel can be compared before one is implemented.

mod chart;
mod full;
mod report;
mod sim;
mod stream;
mod term;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "ui-feel",
    about = "The terminal grammar for `cambi profile`, `cambi affected` and the bench matrix",
    version
)]
struct Cli {
    /// Multiplies every simulated sleep. 0 runs the whole stream instantly.
    #[arg(long, global = true, default_value_t = 1.0)]
    speed: f32,

    /// How the final report reads.
    #[arg(long, global = true, value_enum, default_value = "kv")]
    report: report::Style,

    /// How a commit's sha, date and subject are laid out.
    #[arg(long = "commit", global = true, value_enum, default_value = "compact")]
    commit: report::CommitStyle,

    /// How `affected` draws its targets-by-depth distribution.
    #[arg(long, global = true, value_enum, default_value = "columns")]
    hist: stream::Hist,

    /// Where `affected` finds an index to start from.
    #[arg(long = "index", global = true, value_enum, default_value = "ancestor")]
    index: sim::IndexFound,

    /// How many depth levels `affected` spreads its targets over.
    #[arg(long, global = true, default_value_t = 7)]
    depths: usize,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// `cambi profile` at a terminal: phases rewrite in place, one live line.
    Stream,
    /// The same run with the live line omitted: what a pipe or CI log sees.
    Plain,
    /// `cambi affected` over origin/main..HEAD, answer on stdout.
    Affected {
        /// List every affected target instead of the first 25.
        #[arg(long)]
        all: bool,
        /// Answer the way a commit that invalidates the whole graph does.
        #[arg(long)]
        global: bool,
    },
    /// Quiet run, then two end-of-run plots (rasciigraph and textplots).
    Chart,
    /// Fullscreen bench-matrix dashboard: three subjects by three tools.
    Full,
    /// Every demo in order.
    All,
}

fn main() {
    let cli = Cli::parse();
    let speed = if cli.speed.is_finite() && cli.speed >= 0.0 {
        cli.speed
    } else {
        1.0
    };
    match cli.cmd {
        Cmd::Stream => stream::profile(speed, cli.report, true, cli.commit),
        Cmd::Plain => stream::profile(speed, cli.report, false, cli.commit),
        Cmd::Affected { all, global } => {
            stream::affected(
                speed,
                true,
                all,
                global,
                cli.commit,
                cli.hist,
                cli.index,
                cli.depths,
            )
        }
        Cmd::Chart => chart::run(speed, cli.report),
        Cmd::Full => full::run(speed, cli.report),
        Cmd::All => {
            banner("plain");
            stream::profile(speed, cli.report, false, cli.commit);
            banner("stream");
            stream::profile(speed, cli.report, true, cli.commit);
            banner("affected");
            stream::affected(
                speed,
                true,
                false,
                false,
                cli.commit,
                cli.hist,
                cli.index,
                cli.depths,
            );
            banner("chart");
            chart::run(speed, cli.report);
            banner("full");
            full::run(speed, cli.report);
        }
    }
}

fn banner(name: &str) {
    println!();
    println!("======== {name} ========");
    println!();
}
