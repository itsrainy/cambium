//! `cambi profile`: what cambium costs and saves on this repository, in the
//! sentences someone would put in a proposal.
//!
//! A cold build at the start of the last N first-parent commits, one
//! incremental step per commit forward to the head, and a cold rebuild
//! wherever a commit went global — the benchmark's history workload, run over
//! one worktree in one process. It runs Bazel for minutes, so progress goes
//! to stderr as it goes; the document at the end is
//! [`cambium_engine::Profile`].

use anyhow::{Context as _, Result};
use cambium_engine::profile::{self, Progress, WorktreeRevisions};
use cambium_engine::Profile;
use clap::Args as ClapArgs;

use crate::cmd::{emit, human_bytes, Context};

#[derive(ClapArgs)]
pub struct Args {
    /// The revision the walk ends at. Defaults to HEAD.
    rev: Option<String>,
    /// How many first-parent commits back to walk.
    #[arg(long, default_value_t = 30)]
    commits: usize,
    /// Keep the index the walk ends with here, so `cambi index publish` can
    /// upload it. By default it is written to a scratch file and deleted.
    #[arg(long)]
    out: Option<std::path::PathBuf>,
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    let engine = ctx.engine()?;
    let rev = args.rev.as_deref().unwrap_or("HEAD");
    let head = engine
        .repo()
        .rev_parse(rev)
        .with_context(|| format!("{rev} is not a revision in this repository"))?;
    let (out, _scratch) = match &args.out {
        Some(p) => (p.clone(), None),
        None => {
            std::fs::create_dir_all(engine.cache_dir())?;
            let scratch = tempfile::NamedTempFile::new_in(engine.cache_dir())?;
            (scratch.path().to_path_buf(), Some(scratch))
        }
    };
    let quiet = ctx.render.quiet;
    let mut progress = |p: Progress| {
        if quiet {
            return;
        }
        match p {
            Progress::ColdBuild { commit } => {
                eprintln!(
                    "cambium: cold build at {} (bazel query //...)",
                    short(&commit)
                )
            }
            Progress::Step { index, of, commit } => {
                eprintln!("cambium: [{index}/{of}] {}", short(&commit))
            }
            Progress::Rebuild {
                index,
                commit,
                reason,
            } => eprintln!(
                "cambium: [{index}] {} invalidated everything ({reason}); rebuilding",
                short(&commit)
            ),
        }
    };
    let revs = WorktreeRevisions::create(&engine, &head)?;
    let doc = profile::profile(&engine, &revs, &head, args.commits, &out, &mut progress)?;
    drop(revs);
    for d in &doc.diagnostics {
        ctx.render.warn(d);
    }
    emit(&doc, ctx.format.data("cambi profile")?, |p| {
        print_human(p, engine.workspace(), args.out.as_deref())
    })?;
    Ok(crate::EXIT_OK)
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

/// The paragraph someone pastes into a proposal, then the table behind it.
fn print_human(p: &Profile, workspace: &std::path::Path, kept: Option<&std::path::Path>) {
    let i = &p.index;
    let s = &p.summary;
    println!(
        "Profile of {} at {} (the last {} first-parent commit{}, from {})",
        workspace.display(),
        short(&p.head),
        s.commits,
        plural(s.commits),
        short(&p.from),
    );
    println!();
    println!(
        "This build graph has {} targets ({} tests) in {} packages, with {} external \
         repositor{}.",
        group(i.targets as u64),
        group(i.tests as u64),
        group(i.packages as u64),
        group(i.external_repos as u64),
        if i.external_repos == 1 { "y" } else { "ies" },
    );
    println!(
        "A full cambium index takes {} to build and {} on disk ({}); loading it takes {}.",
        human_ms(p.cold.wall_ms),
        human_bytes(i.bytes_on_disk),
        i.compression,
        human_ms(i.load_ms),
    );
    if s.commits == 0 {
        println!();
        println!(
            "No commit precedes {} on its first-parent line, so nothing was walked.",
            short(&p.head)
        );
        return;
    }
    println!();
    let inc = &s.incremental;
    if inc.commits == 0 {
        println!(
            "Based on the last {} commit{}, every one invalidated the whole graph, so there is \
             no incremental time to report.",
            s.commits,
            plural(s.commits),
        );
    } else {
        println!(
            "Based on the last {} commit{}, cambium takes {} on average to calculate the changed \
             targets for a commit (p50 {}, p90 {}, max {} over the {} commit{} answered \
             incrementally), answering with {:.0} targets on average.",
            s.commits,
            plural(s.commits),
            human_ms(inc.mean_ms),
            human_ms(inc.p50_ms),
            human_ms(inc.p90_ms),
            human_ms(inc.max_ms),
            inc.commits,
            plural(inc.commits),
            s.mean_affected,
        );
    }
    match s.rebuild_every_commits {
        None => println!(
            "The entire graph never had to be rebuilt in those {} commit{}.",
            s.commits,
            plural(s.commits)
        ),
        Some(every) => {
            let reasons: Vec<String> = s
                .global_reasons
                .iter()
                .map(|r| format!("{} × {}", r.count, r.reason))
                .collect();
            println!(
                "The entire graph had to be rebuilt {} time{}, once every {:.1} commits on \
                 average, at {} per rebuild ({}).",
                s.globals,
                plural(s.globals),
                every,
                human_ms(s.rebuild.mean_ms),
                reasons.join("; "),
            );
        }
    }
    if s.engine_cold_rebuilds > 0 {
        println!(
            "{} commit{} exceeded limits.max_dirty_packages and were answered by re-querying \
             the whole repository: exact, at a cold build's cost.",
            s.engine_cold_rebuilds,
            plural(s.engine_cold_rebuilds)
        );
    }

    println!();
    println!("  commit      files  dirty  affected  tests      time");
    for c in &p.commits {
        println!(
            "  {:<10} {:>6} {:>6} {:>9} {:>6} {:>9}{}",
            short(&c.commit),
            c.changed_files,
            c.dirty_packages,
            c.affected,
            c.tests,
            human_ms(c.wall_ms),
            match (&c.global_reason, c.engine_cold_rebuild) {
                (Some(r), _) => format!("  global: {r}"),
                (None, true) => "  re-queried //...".to_string(),
                (None, false) => String::new(),
            }
        );
    }
    if let Some(kept) = kept {
        println!();
        println!(
            "The index for {} is at {}; `cambi index publish {}` uploads it.",
            short(&p.head),
            kept.display(),
            kept.display()
        );
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Thousands separated, which is how a target count is read aloud.
fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Milliseconds in the unit someone would say: `180 ms`, `4.2 s`, `2 min 05 s`.
pub fn human_ms(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms} ms"),
        1000..=59_999 => format!("{:.1} s", ms as f64 / 1000.0),
        _ => format!("{} min {:02} s", ms / 60_000, (ms % 60_000) / 1000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_in_the_unit_a_person_would_use() {
        assert_eq!(human_ms(180), "180 ms");
        assert_eq!(human_ms(4_230), "4.2 s");
        assert_eq!(human_ms(125_000), "2 min 05 s");
    }

    #[test]
    fn counts_are_grouped_by_thousands() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1,000");
        assert_eq!(group(1_234_567), "1,234,567");
    }
}
