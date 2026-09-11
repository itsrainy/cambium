//! `cambium-bench synth` — generated Bazel graphs and workspaces.
//!
//! The generator is deterministic from a preset and a seed, which is what lets
//! a synthetic subject (`synth:<preset>`, see `cambium-bench`) be emitted on
//! demand and compared with itself across machines.

use std::path::PathBuf;

use anyhow::Result;
use cambium_bench::runner;
use clap::Subcommand;

use crate::Context;

#[derive(Subcommand)]
pub enum Args {
    /// Write a real Bazel workspace with git history from a synthetic graph.
    EmitBazel {
        #[arg(long, default_value = "small")]
        spec: String,
        #[arg(long)]
        out: PathBuf,
        /// Change preset driving the generated commits.
        #[arg(long, default_value = "typical-pr")]
        changes: String,
        /// How many commits to generate after the base commit.
        #[arg(long, default_value_t = 5)]
        revisions: u64,
        #[arg(long, default_value_t = 0)]
        seed: u64,
        /// Wipe --out even if it holds something this command did not write.
        /// Without it, only an empty directory or a previously emitted
        /// workspace is replaced.
        #[arg(long)]
        force: bool,
    },
    /// Report the shape of a generated graph without writing anything.
    Describe {
        #[arg(long, default_value = "small")]
        spec: String,
        #[arg(long, default_value_t = 0)]
        seed: u64,
    },
    /// Build an index from a generated graph and write it to a file.
    ///
    /// The graph never goes near Bazel, so this is the cheap way to get an
    /// index of a given size to point `index stats` at.
    BuildIndex {
        #[arg(long, default_value = "small")]
        spec: String,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 0)]
        seed: u64,
    },
    /// List the available graph and change presets.
    Presets,
}

pub fn run(_ctx: &Context, args: Args) -> Result<u8> {
    match args {
        Args::EmitBazel {
            spec,
            out,
            changes,
            revisions,
            seed,
            force,
        } => {
            if force {
                cambium_synth::force_clear(&out)?;
            }
            let repo = runner::emit_bazel(&spec, &changes, revisions, seed, &out)?;
            println!("workspace: {}", repo.dir.display());
            println!("targets:   {}", repo.graph.num_alive_targets());
            println!("packages:  {}", repo.graph.packages.len());
            println!("commits:");
            for (i, c) in repo.commits.iter().enumerate() {
                println!("  {i:>3} {c}");
            }
            println!(
                "\ntry: cambi -C {} affected --base {} --head {}",
                repo.dir.display(),
                repo.commits.first().cloned().unwrap_or_default(),
                repo.commits.last().cloned().unwrap_or_default()
            );
            Ok(crate::EXIT_OK)
        }
        Args::Describe { spec, seed } => {
            let s = cambium_synth::GraphSpec::preset(&spec)
                .ok_or_else(|| anyhow::anyhow!("unknown preset {spec}"))?
                .with_seed(seed);
            let g = cambium_synth::generate(&s);
            let index = g.build_index("describe");
            let c = &index.header.counts;
            println!("preset:     {spec} (spec hash {})", s.hash());
            println!("targets:    {}", g.num_alive_targets());
            println!("packages:   {}", g.packages.len());
            println!("bzl files:  {}", g.bzl.len());
            println!("graph edges:{}", g.num_edges());
            println!("index nodes:{}", c.nodes);
            println!("index edges:{}", c.edges);
            println!("rules:      {}", c.rules);
            println!("tests:      {}", c.tests);
            Ok(crate::EXIT_OK)
        }
        Args::BuildIndex { spec, out, seed } => {
            let s = cambium_synth::GraphSpec::preset(&spec)
                .ok_or_else(|| anyhow::anyhow!("unknown preset {spec}"))?
                .with_seed(seed);
            let g = cambium_synth::generate(&s);
            let index = g.build_index("synthetic");
            cambium_index::format::write_index(&out, &index)?;
            let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            println!("wrote {} ({} bytes)", out.display(), bytes);
            println!(
                "nodes: {} edges: {}",
                index.header.counts.nodes, index.header.counts.edges
            );
            Ok(crate::EXIT_OK)
        }
        Args::Presets => {
            println!("graph presets:");
            for p in cambium_synth::GraphSpec::PRESETS {
                println!("  {p}");
            }
            println!("change presets:");
            for p in cambium_synth::ChangeSpec::PRESETS {
                println!("  {p}");
            }
            Ok(crate::EXIT_OK)
        }
    }
}
