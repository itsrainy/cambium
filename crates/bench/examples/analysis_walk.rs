//! The `skyframe-analysis` tier, off the harness.
//!
//! The harness measures the strategy over git steps. The escalation
//! experiments in docs/SKYFRAME_PROTOTYPES.md need the same two halves over
//! *synthetic* edits instead (a toolchain BUILD, a `.bazelrc` flag, a
//! `bazel_dep` bump), so the parser, the graph and the reverse walk are
//! reachable here without a step to hang them on.
//!
//! ```console
//! $ bazel dump --skyframe=deps --skykey_filter=… > dump.txt
//! $ analysis_walk build dump.txt graph.zst
//! $ analysis_walk walk graph.zst /path/to/workspace tools/foo/BUILD .bazelrc
//! ```
//!
//! `walk` prints the answer size and the first labels of it, which is what
//! the experiments compare against `//...`.

use std::collections::BTreeSet;

use cambium_bench::skyframe::analysis::{dump, graph, Graph};
use cambium_bench::skyframe::ChangedPath;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("build") => {
            let (input, out) = (&args[1], &args[2]);
            let parsed = dump::parse(std::io::BufReader::with_capacity(
                1 << 20,
                std::fs::File::open(input)?,
            ))?;
            println!(
                "nodes {} edges {} unparsed {}",
                parsed.nodes.len(),
                parsed.edges,
                parsed.unparsed
            );
            let g = Graph::from_dump(parsed);
            let by_label = g.configs_by_label();
            let checksums: BTreeSet<&str> = by_label.values().flatten().copied().collect();
            println!(
                "configured labels {} (main repo {}), config checksums {}, labels under more \
                 than one config {}",
                by_label.len(),
                by_label.keys().filter(|l| l.starts_with("//")).count(),
                checksums.len(),
                by_label.values().filter(|s| s.len() > 1).count()
            );
            for c in &checksums {
                println!("  config {c}");
            }
            println!("graph bytes {}", g.write(std::path::Path::new(out))?);
        }
        Some("walk") => {
            let g = Graph::read(std::path::Path::new(&args[1]))?;
            let workspace = &args[2];
            let changed: Vec<ChangedPath> = args[3..]
                .iter()
                .map(|p| ChangedPath {
                    status: "M".into(),
                    path: p.clone(),
                })
                .collect();
            let packages = g.main_repo_packages();
            let (seeds, routes) = graph::seeds(&g, workspace, &changed, &packages);
            let reached = g.reach(&seeds);
            let labels = g.answer_labels(&reached);
            println!("seeds {} {routes:?}", seeds.len());
            println!("reached nodes {}", reached.len());
            println!("answer {}", labels.len());
            for l in labels.iter().take(20) {
                println!("  {l}");
            }
        }
        _ => {
            eprintln!(
                "usage: analysis_walk build <dump.txt> <graph.zst>\n       \
                 analysis_walk walk <graph.zst> <workspace> <changed path>..."
            );
            std::process::exit(2);
        }
    }
    Ok(())
}
