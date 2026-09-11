//! `cambi index build|update|publish|list|show|gc`: the stored indexes.
//!
//! The engine still calls the stored artifact a baseline (`[baseline].store`,
//! `EngineError::BaselineUnavailable`); the surface calls it an index, which
//! is what it is. `cambi baseline ...` stays for one release as a deprecated
//! spelling of these verbs.

use anyhow::{Context as _, Result};
use cambium_engine::{AffectedRequest, OnGlobal, PublishPolicy, PublishSource};
use clap::Subcommand;

use crate::cmd::{hex, Context};

#[derive(Subcommand)]
pub enum Args {
    /// Build an index from scratch with `bazel query //...`, minting a new lineage.
    Build {
        /// Revision the index describes. Defaults to HEAD.
        rev: Option<String>,
        /// The old spelling of the revision; `cambi index build <rev>` now.
        #[arg(long, hide = true, conflicts_with = "rev")]
        commit: Option<String>,
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
    /// Bring an index forward from one revision to another.
    Update {
        /// The range, spelled as `git diff` spells it: `FROM..TO`, or a single
        /// revision for `<rev>..HEAD`. The index for FROM is fetched or built;
        /// the result describes TO.
        #[arg(value_name = "FROM..TO", conflicts_with_all = ["from", "to"])]
        range: Option<String>,
        /// The old spelling of the range's left half; `cambi index update
        /// FROM..TO` now.
        #[arg(long, hide = true)]
        from: Option<String>,
        /// The old spelling of the range's right half.
        #[arg(long, hide = true)]
        to: Option<String>,
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
    /// Upload an index file to the configured store.
    ///
    /// The index says which lineage and which commit it is for, so this takes
    /// nothing else. `build --publish` / `update --publish` are the same
    /// upload from a file the run itself produced.
    Publish {
        /// The index file to upload.
        file: std::path::PathBuf,
    },
    /// List published indexes.
    List,
    /// Print one index's header and counts.
    Show {
        /// The index file to read.
        file: std::path::PathBuf,
    },
    /// Delete published indexes older than the configured retention.
    Gc {
        /// Keep this artifact whatever its age.
        #[arg(long)]
        keep: Option<String>,
    },
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    // `show` reads a file and nothing else, so it must work outside a
    // workspace: it is the one verb that opens no engine. The others open
    // theirs as their first step.
    match args {
        Args::Show { file } => crate::cmd::inspect::show(ctx, &file),
        Args::Build {
            rev,
            commit,
            publish,
            out,
            allow_partial,
        } => {
            let rev = rev.or(commit).unwrap_or_else(|| "HEAD".to_string());
            let engine = ctx.engine()?;
            let sha = engine
                .repo()
                .rev_parse(&rev)
                .with_context(|| format!("{rev} is not a revision in this repository"))?;
            let index = engine.full_build(&sha)?;
            emit(
                &engine,
                &index,
                out.as_deref(),
                publish.then_some(PublishPolicy { allow_partial }),
            )?;
            Ok(crate::EXIT_OK)
        }
        Args::Update {
            range,
            from,
            to,
            publish,
            out,
            no_rebuild_on_global,
            allow_partial,
        } => {
            // A missing range is the caller's mistake, whatever the workspace's
            // state, so it is reported before the workspace is opened.
            let (from, to) = update_range(range, from, to)?;
            let engine = ctx.engine()?;
            let to_sha = engine
                .repo()
                .rev_parse(&to)
                .with_context(|| format!("{to} is not a revision in this repository"))?;
            let req = AffectedRequest {
                base: from,
                head: to_sha,
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
            let on_global = if no_rebuild_on_global {
                OnGlobal::Fail
            } else {
                OnGlobal::Rebuild
            };
            let updated = engine.update_index(&req, on_global)?;
            if let Some(reason) = &updated.rebuilt_because {
                eprintln!(
                    "cambium: range invalidates everything ({reason}); rebuilt a fresh lineage"
                );
            }
            emit(
                &engine,
                &updated.index,
                out.as_deref(),
                publish.then_some(PublishPolicy { allow_partial }),
            )?;
            Ok(crate::EXIT_OK)
        }
        Args::Gc { keep } => {
            let engine = ctx.engine()?;
            let store = engine.store().context("no baseline store is configured")?;
            let removed = store.gc(
                engine.config().baseline.retention_days,
                keep.as_deref().unwrap_or(""),
            )?;
            println!("removed {removed} index files");
            Ok(crate::EXIT_OK)
        }
        Args::Publish { file } => {
            let engine = ctx.engine()?;
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
            let engine = ctx.engine()?;
            let store = engine.store().context("no baseline store is configured")?;
            for n in store.list()? {
                println!("{n}");
            }
            Ok(crate::EXIT_OK)
        }
    }
}

/// Writes the index to `out`, and publishes it when `publish` says how.
fn emit(
    engine: &cambium_engine::Engine,
    index: &cambium_index::IndexData,
    out: Option<&std::path::Path>,
    publish: Option<PublishPolicy>,
) -> Result<()> {
    // An index is routinely gigabytes. Writing one into `temp_dir()` under a
    // content-addressed name and walking away leaves it there for good, so
    // the only run allowed to skip `--out` is one that publishes, and that
    // one writes through a `NamedTempFile` that is deleted on the way out.
    // The scratch file lives as long as this function does, which is as long
    // as the publish needs it.
    let (path, _scratch) = match (out, publish) {
        (None, None) => anyhow::bail!(
            "pass --out <file> to say where the index goes, or --publish to upload it"
        ),
        (None, Some(_)) => {
            let scratch = tempfile::NamedTempFile::new()?;
            (scratch.path().to_path_buf(), Some(scratch))
        }
        (Some(p), _) => (p.to_path_buf(), None),
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
    if let Some(policy) = publish {
        let names = engine.publish(
            PublishSource::File {
                path: &path,
                lineage: &index.header.lineage,
                commit: &index.provenance.commit,
            },
            policy,
        )?;
        for n in names {
            eprintln!("cambium: published {n}");
        }
    }
    Ok(())
}

/// The two revisions `index update` moves between: the positional range, or
/// the old `--from`/`--to`. Unlike `affected`, there is no merge-base default:
/// bringing an index forward from nowhere is not a thing.
fn update_range(
    range: Option<String>,
    from: Option<String>,
    to: Option<String>,
) -> Result<(String, String)> {
    if let Some(range) = range {
        if range.contains("...") {
            return Err(crate::diag::UsageError::raised(format!(
                "`{range}`: cambi takes a two-dot range (FROM..TO); a bare revision already \
                 means <rev>..HEAD"
            )));
        }
        return Ok(match range.split_once("..") {
            Some((a, b)) => (a.to_string(), b.to_string()),
            None => (range, "HEAD".to_string()),
        });
    }
    match from {
        Some(from) => Ok((from, to.unwrap_or_else(|| "HEAD".to_string()))),
        None => Err(crate::diag::UsageError::raised(
            "cambi index update needs a range: `cambi index update FROM..TO`, or FROM alone \
             for FROM..HEAD",
        )),
    }
}

#[cfg(test)]
mod range_tests {
    use super::update_range;

    #[test]
    fn a_range_splits_and_a_bare_revision_means_head() {
        assert_eq!(
            update_range(Some("a..b".into()), None, None).unwrap(),
            ("a".into(), "b".into())
        );
        assert_eq!(
            update_range(Some("a".into()), None, None).unwrap(),
            ("a".into(), "HEAD".into())
        );
        assert_eq!(
            update_range(None, Some("a".into()), None).unwrap(),
            ("a".into(), "HEAD".into())
        );
        assert!(update_range(None, None, None).is_err());
        assert!(update_range(Some("a...b".into()), None, None).is_err());
    }
}
