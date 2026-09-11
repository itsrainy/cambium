//! `cambi index build|update|publish|list|show|gc`: the stored indexes.
//!
//! The engine still calls the stored artifact a baseline (`[baseline].store`,
//! `EngineError::BaselineUnavailable`); the surface calls it an index, which
//! is what it is. `cambi baseline ...` stays for one release as a deprecated
//! spelling of these verbs.

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
        /// Write the index here. Required unless `--publish`, which can
        /// upload from a temporary file that is deleted afterwards.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Publish even if `bazel query` could not load every package.
        #[arg(long)]
        allow_partial: bool,
    },
    /// Bring a baseline forward from one revision to another.
    Update {
        #[arg(long)]
        from: String,
        #[arg(long, default_value = "HEAD")]
        to: String,
        #[arg(long)]
        publish: bool,
        /// Write the index here. Required unless `--publish`.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Publish even if `bazel query` could not load every package.
        #[arg(long)]
        allow_partial: bool,
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
    /// Upload an index file to the configured store.
    ///
    /// The index says which lineage and which commit it is for, so this takes
    /// nothing else. `build --publish` / `update --publish` are the same
    /// upload from a file the run itself produced.
    Publish {
        /// The index file to upload.
        file: std::path::PathBuf,
    },
    /// Print one index's header and counts.
    Show {
        /// The index file to read.
        file: std::path::PathBuf,
    },
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    // `show` reads a file and nothing else: it must work outside a workspace,
    // so it is answered before the engine is opened.
    if let Args::Show { file } = &args {
        return crate::cmd::inspect::show(ctx, file);
    }
    let mut engine = ctx.engine()?;
    match args {
        Args::Build {
            commit,
            publish,
            out,
            allow_partial,
        } => {
            engine.cfg.baseline.publish_partial |= allow_partial;
            let sha = engine.repo.rev_parse(&commit).with_context(|| {
                format!("--commit {commit} is not a revision in this repository")
            })?;
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
            allow_partial,
        } => {
            engine.cfg.baseline.publish_partial |= allow_partial;
            let to_sha = engine
                .repo
                .rev_parse(&to)
                .with_context(|| format!("--to {to} is not a revision in this repository"))?;
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
                allow_partial,
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
        Args::Show { .. } => unreachable!("answered before the engine is opened"),
        Args::Publish { file } => {
            let f = cambium_index::IndexFile::open(&file)?;
            let v = f.view();
            let (lineage, commit) = (v.lineage, v.commit.to_string());
            let names = engine.publish_index(&file, &lineage, &commit)?;
            for n in names {
                eprintln!("cambium: published {n}");
            }
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
    // An index is routinely gigabytes. Writing one into `temp_dir()` under a
    // content-addressed name and walking away leaves it there for good, so
    // the only run allowed to skip `--out` is one that publishes, and that
    // one writes through a `NamedTempFile` that is deleted on the way out.
    let scratch = match (out, publish) {
        (None, false) => anyhow::bail!(
            "pass --out <file> to say where the index goes, or --publish to upload it"
        ),
        (None, true) => Some(tempfile::NamedTempFile::new()?),
        (Some(_), _) => None,
    };
    let path = match (out, &scratch) {
        (Some(p), _) => p.to_path_buf(),
        (None, Some(t)) => t.path().to_path_buf(),
        (None, None) => unreachable!("the None/false arm bailed"),
    };
    cambium_index::format::write_index_with(&path, index, engine.index_compression())?;
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
