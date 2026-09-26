//! `cambi index build|update|publish|list|show|gc`: the stored indexes.
//!
//! The engine still calls the stored artifact a baseline (`[baseline].store`,
//! `EngineError::BaselineUnavailable`); the surface calls it an index, which
//! is what it is. `cambi baseline ...` stays for one release as a deprecated
//! spelling of these verbs.

use anyhow::{Context as _, Result};
use cambium_engine::{AffectedRequest, EngineError, OnGlobal, PublishPolicy, PublishSource};
use clap::Subcommand;

use std::time::Instant;

use cambium_engine::{Code, Diagnostic};
use cambium_ui::report::{group, human_bytes};
use cambium_ui::{Event, Level, Sink};

use crate::cmd::narrate::repo_name;
use crate::cmd::{hex, short, Context};

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
    /// Delete stale lineages, and published indexes older than the
    /// configured retention. A directory store only.
    ///
    /// A lineage is stale when this cambi cannot splice onto it: built with
    /// another cambium.toml (or an older rule-hash version or index content
    /// revision), or written in an older index format. Its index files and
    /// commit pointers all go, whatever their age.
    Gc {
        /// Keep this artifact whatever its age or lineage.
        #[arg(long)]
        keep: Option<String>,
        /// Keep stale lineages, and delete by age alone: for a store that
        /// another configuration or an older cambi still reads from.
        #[arg(long)]
        keep_incompatible: bool,
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
            let publish = publish.then_some(PublishPolicy { allow_partial });
            check_destination(out.as_deref(), publish)?;
            let engine = ctx.engine()?;
            let sha = engine
                .repo()
                .rev_parse(&rev)
                .with_context(|| format!("{rev} is not a revision in this repository"))?;
            let mut sink = cambium_ui::boxed(ctx.ui);
            sink.emit(Event::Header {
                verb: "index build".into(),
                repo: repo_name(&engine),
                range: short(&rev),
                note: String::new(),
            });
            sink.emit(Event::PhaseStart {
                name: "index".into(),
                detail: Some(format!("bazel query //... at {}", short(&sha))),
            });
            sink.emit(Event::Current(format!("indexing {}", short(&sha))));
            let started = Instant::now();
            let built = engine.full_build(&sha);
            let index = finish_or_say(ctx, &engine, sink.as_mut(), built)?;
            sink.emit(Event::PhaseEnd {
                name: "index".into(),
                result: counts(&index),
                elapsed_ms: ms(started),
            });
            let said = engine.diagnostics().drain();
            emit(ctx, &engine, sink, &index, out.as_deref(), publish, said)?;
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
            let publish = publish.then_some(PublishPolicy { allow_partial });
            check_destination(out.as_deref(), publish)?;
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
            let mut sink = cambium_ui::boxed(ctx.ui);
            sink.emit(Event::Header {
                verb: "index update".into(),
                repo: repo_name(&engine),
                range: format!("{}..{}", short(&req.base), short(&to)),
                note: String::new(),
            });
            sink.emit(Event::PhaseStart {
                name: "index".into(),
                detail: Some(format!(
                    "bringing {} forward to {}",
                    short(&req.base),
                    short(&req.head)
                )),
            });
            sink.emit(Event::Current(format!(
                "bringing {} forward",
                short(&req.base)
            )));
            let started = Instant::now();
            let updated = engine.update_index(&req, on_global);
            let updated = finish_or_say(ctx, &engine, sink.as_mut(), updated)?;
            if let Some(reason) = &updated.rebuilt_because {
                sink.emit(Event::Log(
                    Level::Warning,
                    format!(
                        "the range invalidates everything ({reason}); rebuilt from scratch, \
                         minting a fresh lineage"
                    ),
                ));
            }
            sink.emit(Event::PhaseEnd {
                name: "index".into(),
                result: counts(&updated.index),
                elapsed_ms: ms(started),
            });
            let mut said = updated.diagnostics.clone();
            cambium_engine::diag::extend_unique(&mut said, engine.diagnostics().drain());
            emit(
                ctx,
                &engine,
                sink,
                &updated.index,
                out.as_deref(),
                publish,
                said,
            )?;
            Ok(crate::EXIT_OK)
        }
        Args::Gc {
            keep,
            keep_incompatible,
        } => {
            let engine = ctx.engine()?;
            let got = engine.collect_garbage(keep.as_deref().unwrap_or(""), keep_incompatible)?;
            let mut sink = cambium_ui::boxed(ctx.ui);
            sink.emit(Event::Header {
                verb: "index gc".into(),
                repo: repo_name(&engine),
                range: String::new(),
                note: format!("retention {} days", engine.config().baseline.retention_days),
            });
            for (s, n) in &got.stale {
                sink.emit(Event::Log(
                    Level::Info,
                    format!("lineage {}: {}; removed {}", s.lineage, s.why, files(*n)),
                ));
            }
            sink.emit(Event::Log(
                Level::Info,
                format!("older than the retention: removed {}", files(got.aged)),
            ));
            sink.emit(Event::Done);
            println!("removed {} index files", got.total());
            Ok(crate::EXIT_OK)
        }
        Args::Publish { file } => {
            let engine = ctx.engine()?;
            // No store is E008 before the header, as `index gc` does: a
            // header announcing a publish that cannot start reads as one
            // that started and failed.
            configured_store(&engine)?;
            let f = cambium_index::IndexFile::open(&file)?;
            let v = f.view();
            let (lineage, commit) = (v.lineage, v.commit.to_string());
            let mut sink = cambium_ui::boxed(ctx.ui);
            sink.emit(Event::Header {
                verb: "index publish".into(),
                repo: repo_name(&engine),
                range: short(&commit),
                note: format!("lineage {}", hex(&lineage)),
            });
            let started = Instant::now();
            let names = engine.publish_index(&file, &lineage, &commit);
            let names = finish_or_say(ctx, &engine, sink.as_mut(), names)?;
            say_published(sink.as_mut(), &names, started);
            sink.emit(Event::Done);
            Ok(crate::EXIT_OK)
        }
        Args::List => {
            let engine = ctx.engine()?;
            for n in configured_store(&engine)?
                .list()
                .map_err(EngineError::from)?
            {
                println!("{n}");
            }
            Ok(crate::EXIT_OK)
        }
    }
}

/// `1 file`, `3 files`.
fn files(n: usize) -> String {
    format!("{n} file{}", if n == 1 { "" } else { "s" })
}

/// The store `[baseline].store` names, or `CAMBI-E008` saying there is none.
///
/// A store failure goes through [`EngineError`] too (`.map_err` at each
/// call), so it keeps its own code: `CAMBI-E012` for an operation the backend
/// has no implementation of, `CAMBI-E010`/`E011` and exit 5 for a store that
/// is there and failed, never the generic `CAMBI-E001`.
fn configured_store(
    engine: &cambium_engine::Engine,
) -> Result<&dyn cambium_engine::store::ArtifactStore> {
    Ok(engine.store().ok_or(EngineError::NoStore)?)
}

/// `--out`, `--publish`, or both: an index is routinely gigabytes, and one
/// written into `temp_dir()` and walked away from stays there for good, so
/// the only run allowed to skip `--out` is one that publishes. Checked before
/// anything is built, so a forgotten flag costs nothing.
fn check_destination(out: Option<&std::path::Path>, publish: Option<PublishPolicy>) -> Result<()> {
    if out.is_none() && publish.is_none() {
        return Err(crate::diag::UsageError::raised_with_help(
            "the index would go nowhere",
            "pass --out <file> to say where the index goes, or --publish to upload it",
        ));
    }
    Ok(())
}

/// The result of the step the narration is inside, or, on failure, the
/// narration closed and what the engine said on the way printed before the
/// error the caller returns.
fn finish_or_say<T>(
    ctx: &Context,
    engine: &cambium_engine::Engine,
    sink: &mut dyn Sink,
    r: cambium_engine::Result<T>,
) -> Result<T> {
    if r.is_err() {
        sink.emit(Event::Done);
        for d in engine.diagnostics().drain() {
            ctx.render.warn(&d);
        }
    }
    Ok(r?)
}

/// Writes the index to `out`, and publishes it when `publish` says how,
/// narrating both through `sink`; then says the run's diagnostics, `said`.
fn emit(
    ctx: &Context,
    engine: &cambium_engine::Engine,
    mut sink: Box<dyn Sink>,
    index: &cambium_index::IndexData,
    out: Option<&std::path::Path>,
    publish: Option<PublishPolicy>,
    mut said: Vec<Diagnostic>,
) -> Result<()> {
    // Loud, every time: an index that is missing packages will happily
    // answer `affected` queries about the ones it has, and nothing
    // downstream can tell the difference without being told. The query that
    // built it has usually said so already; an index brought forward onto a
    // partial one has not.
    if let Some(w) = cambium_engine::inspect::PartialLoad::from(index.view()).warning() {
        if !said.iter().any(|d| d.code == Code::W006PartialQuery) {
            said.push(Diagnostic::warning(Code::W006PartialQuery, w).with_help(
                "fix the load errors, or name the packages whose absence is expected in \
                 baseline.publish_partial (--allow-partial for one run)",
            ));
        }
    }
    // The scratch file lives as long as this function does, which is as long
    // as the publish needs it, and is deleted on the way out.
    let (path, scratch) = match out {
        Some(p) => (p.to_path_buf(), None),
        None => {
            let scratch = tempfile::NamedTempFile::new()?;
            (scratch.path().to_path_buf(), Some(scratch))
        }
    };
    let started = Instant::now();
    let written = cambium_index::format::write_index_with(&path, index, engine.index_compression());
    finish_or_say(
        ctx,
        engine,
        sink.as_mut(),
        written.map_err(EngineError::from),
    )?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    sink.emit(Event::PhaseEnd {
        name: "write".into(),
        result: match &scratch {
            Some(_) => format!("{} for the upload", human_bytes(size)),
            None => format!("{} to {}", human_bytes(size), path.display()),
        },
        elapsed_ms: ms(started),
    });
    if let Some(policy) = publish {
        let started = Instant::now();
        let names = engine.publish(
            PublishSource::File {
                path: &path,
                lineage: &index.header.lineage,
                commit: &index.provenance.commit,
            },
            policy,
        );
        let names = finish_or_say(ctx, engine, sink.as_mut(), names)?;
        say_published(sink.as_mut(), &names, started);
    }
    sink.emit(Event::Done);
    drop(scratch);
    cambium_engine::diag::extend_unique(&mut said, engine.diagnostics().drain());
    for d in &said {
        ctx.render.warn(d);
    }
    Ok(())
}

/// The `publish` phase: how many keys, and each one on its own line.
fn say_published(sink: &mut dyn Sink, names: &[String], started: Instant) {
    sink.emit(Event::PhaseEnd {
        name: "publish".into(),
        result: format!(
            "{} key{}",
            names.len(),
            if names.len() == 1 { "" } else { "s" }
        ),
        elapsed_ms: ms(started),
    });
    for n in names {
        sink.emit(Event::Log(Level::Info, n.clone()));
    }
}

/// An index's size and lineage, as the `index` phase reports it.
fn counts(index: &cambium_index::IndexData) -> String {
    let c = &index.header.counts;
    format!(
        "{} nodes, {} edges, {} packages, lineage {}",
        group(c.nodes as u64),
        group(c.edges),
        group(c.packages as u64),
        hex(&index.header.lineage)
    )
}

fn ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
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
