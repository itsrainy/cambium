//! `cambium index inspect|verify`.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use cambium_index::{walk, IndexFile, NodeKind, WalkOptions};
use clap::Subcommand;

use crate::cmd::{hex, Context};

#[derive(Subcommand)]
pub enum Args {
    /// Print an index's header, counts, and optionally a target or package.
    Inspect {
        file: PathBuf,
        /// Show one target's row: kind, package, hash, deps and rdeps.
        #[arg(long)]
        label: Option<String>,
        /// Show one package's targets.
        #[arg(long)]
        package: Option<String>,
        /// Show the targets a change to `label` would affect.
        #[arg(long)]
        rdeps: Option<String>,
    },
    /// Check an index against Bazel (spec 12.3): sample rule targets, compare
    /// our reverse closure with `bazel query rdeps(//..., label)`.
    Verify {
        file: PathBuf,
        /// How many targets to sample.
        #[arg(long, default_value_t = 200)]
        sample: usize,
        /// Seed for the sample, so a failure can be reproduced.
        #[arg(long, default_value_t = 0)]
        seed: u64,
        #[arg(long = "against-bazel", default_value_t = true)]
        against_bazel: bool,
    },
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    match args {
        Args::Inspect {
            file,
            label,
            package,
            rdeps,
        } => inspect(&file, label, package, rdeps),
        Args::Verify {
            file,
            sample,
            seed,
            against_bazel,
        } => {
            if !against_bazel {
                anyhow::bail!("index verify currently only supports --against-bazel");
            }
            crate::cmd::index::verify(ctx, &file, sample, seed)
        }
    }
}

fn inspect(
    file: &PathBuf,
    label: Option<String>,
    package: Option<String>,
    rdeps: Option<String>,
) -> Result<u8> {
    let f = IndexFile::open(file)?;
    let v = f.view();
    println!("file:           {}", file.display());
    println!("format version: {}", v.format_version);
    println!("commit:         {}", v.commit);
    println!("lineage:        {}", hex(&v.lineage));
    println!("bazel:          {}", v.bazel_version);
    println!("config hash:    {}", hex(&v.config_hash));
    println!("created:        {}", v.created_unix);
    println!("size on disk:   {} bytes", f.file_size());
    println!("nodes:          {}", v.num_nodes());
    println!("edges:          {}", v.num_edges());
    println!("packages:       {}", v.num_packages());
    let hist = cambium_engine::engine::kind_histogram(v);
    let mut kinds: Vec<_> = hist.into_iter().collect();
    kinds.sort();
    for (k, n) in kinds {
        println!("  {k:<18} {n}");
    }

    if let Some(l) = label {
        let map = v.labels.build_map();
        let id = *map.get(l.as_str()).context("label not found in index")?;
        println!("\n{l}");
        println!("  id:         {id}");
        println!("  kind:       {:?}", v.kind_of(id));
        println!("  package:    {:?}", v.package_of(id));
        println!("  rule class: {:?}", v.rule_class(id));
        println!("  is_test:    {}", v.is_test[id as usize]);
        println!("  manual:     {}", v.is_manual(id));
        println!("  hash:       {}", hex(&v.hash[id as usize]));
        println!("  deps ({}):", v.forward.row_len(id));
        for &d in v.forward.row(id) {
            println!("    {}", v.label(d));
        }
        println!("  rdeps ({}):", v.reverse.row_len(id));
        for &d in v.reverse.row(id) {
            println!("    {}", v.label(d));
        }
    }

    if let Some(p) = package {
        let pid = (0..v.num_packages())
            .find(|&i| v.package_name(i) == p)
            .context("package not found in index")?;
        println!("\npackage //{p}");
        println!("  build hash: {}", hex(&v.pkg_build_hash[pid as usize]));
        println!("  tombstoned: {}", v.package_tombstoned(pid));
        for &id in v.package_nodes(pid) {
            println!("    {:<14} {}", format!("{:?}", v.kind_of(id)), v.label(id));
        }
    }

    if let Some(l) = rdeps {
        let map = v.labels.build_map();
        let id = *map.get(l.as_str()).context("label not found in index")?;
        let r = walk::walk(v, &[id], &WalkOptions::default());
        println!("\n{} targets affected by a change to {l}:", r.affected.len());
        for lbl in walk::labels_sorted(v, &r.affected) {
            println!("  {lbl}");
        }
    }

    Ok(crate::EXIT_OK)
}

/// Differential check against Bazel (spec 12.3).
///
/// For a random sample of rule targets, our reverse closure must be a superset
/// of `bazel query rdeps(//..., label)`. Missing targets are soundness bugs and
/// fail the check; extra targets are the over-approximation the design accepts,
/// and are reported as a precision percentage.
fn verify(ctx: &Context, file: &PathBuf, sample: usize, seed: u64) -> Result<u8> {
    use std::process::Command;

    let engine = ctx.engine()?;
    let f = IndexFile::open(file)?;
    let v = f.view();

    let rules: Vec<u32> = (0..v.num_nodes())
        .filter(|&i| v.kind_of(i) == NodeKind::Rule && !v.is_tombstone(i))
        .collect();
    if rules.is_empty() {
        anyhow::bail!("index contains no rule targets");
    }

    // Deterministic sample: stride through the rule ids from a seeded offset.
    let stride = (rules.len() / sample.max(1)).max(1);
    let start = (seed as usize) % rules.len();
    let picked: Vec<u32> = (0..sample.min(rules.len()))
        .map(|i| rules[(start + i * stride) % rules.len()])
        .collect();

    let mut checked = 0usize;
    let mut unsound = 0usize;
    let mut ours_total = 0usize;
    let mut theirs_total = 0usize;

    for id in picked {
        let label = v.label(id);
        let out = Command::new(engine.bazel.binary())
            .current_dir(engine.workspace())
            .args([
                "query",
                &format!("rdeps(//..., {label})"),
                "--output=label",
                "--noimplicit_deps",
                "--order_output=no",
                "--keep_going",
                "--noshow_progress",
                "--ui_event_filters=-info",
            ])
            .output()?;
        if !out.status.success() {
            eprintln!("cambium: skipping {label}: bazel query failed");
            continue;
        }
        let theirs: std::collections::HashSet<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let r = walk::walk(
            v,
            &[id],
            &WalkOptions {
                include_manual: true,
                explain: false,
            },
        );
        let ours: std::collections::HashSet<String> =
            walk::labels_sorted(v, &r.affected).into_iter().collect();

        let missing: Vec<&String> = theirs.difference(&ours).collect();
        checked += 1;
        ours_total += ours.len();
        theirs_total += theirs.len();
        if !missing.is_empty() {
            unsound += 1;
            eprintln!(
                "UNSOUND {label}: {} targets Bazel reports as rdeps are missing from the index",
                missing.len()
            );
            for m in missing.iter().take(10) {
                eprintln!("    {m}");
            }
        }
    }

    let precision = if ours_total == 0 {
        100.0
    } else {
        theirs_total as f64 / ours_total as f64 * 100.0
    };
    println!("checked {checked} targets against bazel");
    println!("unsound: {unsound}");
    println!("precision: {precision:.1}% ({theirs_total} of {ours_total} reported targets are real rdeps)");
    Ok(if unsound == 0 {
        crate::EXIT_OK
    } else {
        crate::EXIT_ERROR
    })
}
