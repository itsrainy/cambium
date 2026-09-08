//! `cambi baseline build|update|gc` (spec 8).

use anyhow::{Context as _, Result};
use cambium_engine::AffectedRequest;
use clap::Subcommand;

use crate::cmd::{hex, Context};

#[derive(Subcommand)]
pub enum Args {
    /// Build an index from scratch with `bazel query //...`, minting a new lineage.
    Build {
        /// Revision the index describes. Defaults to HEAD.
        #[arg(long, default_value = "HEAD")]
        commit: String,
        /// Upload the result to the configured store.
        #[arg(long)]
        publish: bool,
        /// Also write the index here.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// Bring a baseline forward from one revision to another.
    Update {
        #[arg(long)]
        from: String,
        #[arg(long, default_value = "HEAD")]
        to: String,
        #[arg(long)]
        publish: bool,
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Fail instead of rebuilding from scratch when the range invalidates
        /// everything. By default a global invalidation mints a fresh
        /// lineage.
        #[arg(long)]
        no_rebuild_on_global: bool,
    },
    /// Delete published indexes older than the configured retention.
    Gc {
        /// Keep this artifact whatever its age.
        #[arg(long)]
        keep: Option<String>,
    },
    /// List published indexes.
    List,
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    let engine = ctx.engine()?;
    match args {
        Args::Build {
            commit,
            publish,
            out,
        } => {
            let sha = engine.repo.rev_parse(&commit).unwrap_or(commit);
            let index = engine.full_build(&sha)?;
            emit(&engine, &index, out.as_deref(), publish)?;
            Ok(crate::EXIT_OK)
        }
        Args::Update {
            from,
            to,
            publish,
            out,
            no_rebuild_on_global,
        } => {
            let to_sha = engine.repo.rev_parse(&to).unwrap_or(to);
            let req = AffectedRequest {
                base: from,
                head: to_sha.clone(),
                include_manual: false,
                explain: false,
                // `baseline update` publishes its output, so it needs one.
                materialize: true,
                no_full_build: false,
                // `baseline update` publishes its own output below; there is
                // nothing for the base-side publish to add.
                publish_base: false,
                max_depth: None,
            };
            let outcome = engine.affected(&req)?;
            let index = match outcome.index {
                Some(i) => i,
                None => {
                    if no_rebuild_on_global {
                        anyhow::bail!(
                            "the range invalidates everything ({}); drop --no-rebuild-on-global \
                             to mint a fresh lineage instead",
                            outcome
                                .report
                                .global_reason
                                .unwrap_or_else(|| "global".into())
                        );
                    }
                    eprintln!(
                        "cambium: range invalidates everything ({}); rebuilding a fresh lineage",
                        outcome
                            .report
                            .global_reason
                            .clone()
                            .unwrap_or_else(|| "global".into())
                    );
                    engine.full_build(&to_sha)?
                }
            };
            emit(&engine, &index, out.as_deref(), publish)?;
            Ok(crate::EXIT_OK)
        }
        Args::Gc { keep } => {
            let store = engine
                .store
                .as_ref()
                .context("no baseline store is configured")?;
            let removed = store.gc(
                engine.cfg.baseline.retention_days,
                keep.as_deref().unwrap_or(""),
            )?;
            println!("removed {removed} index files");
            Ok(crate::EXIT_OK)
        }
        Args::List => {
            let store = engine
                .store
                .as_ref()
                .context("no baseline store is configured")?;
            for n in store.list()? {
                println!("{n}");
            }
            Ok(crate::EXIT_OK)
        }
    }
}

fn emit(
    engine: &cambium_engine::Engine,
    index: &cambium_index::IndexData,
    out: Option<&std::path::Path>,
    publish: bool,
) -> Result<()> {
    let key = cambium_index::index_key(&index.header.lineage, &index.provenance.commit);
    let name = key.rsplit('/').next().unwrap_or(&key).to_string();
    let path = match out {
        Some(p) => p.to_path_buf(),
        None => std::env::temp_dir().join(&name),
    };
    cambium_index::format::write_index(&path, index)?;
    let c = &index.header.counts;
    eprintln!(
        "cambium: wrote {} ({} nodes, {} edges, {} packages, lineage {})",
        path.display(),
        c.nodes,
        c.edges,
        c.packages,
        hex(&index.header.lineage)
    );
    // Loud, on stderr, every time: an index that is missing packages will
    // happily answer `affected` queries about the ones it has, and nothing
    // downstream can tell the difference without being told.
    if let Some(w) = cambium_engine::inspect::PartialLoad::from(index.view()).warning() {
        eprintln!("cambium: WARNING: {w}");
    }
    if publish {
        let names = engine.publish_index(&path, &index.header.lineage, &index.provenance.commit)?;
        for n in names {
            eprintln!("cambium: published {n}");
        }
    }
    Ok(())
}
