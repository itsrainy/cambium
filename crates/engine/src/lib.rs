//! cambium's orchestration layer.
//!
//! A run is: load a baseline index, ask git what changed, turn that into a set
//! of dirty packages, re-query only those with Bazel, splice their rows into
//! the index, derive the direct set, and walk the reverse edges. Every step is
//! proportional to the size of the change or the size of the answer — except
//! the splice memcpy, which is proportional to the graph but measured in tens
//! of milliseconds.

pub mod baseline;
pub mod bazelrc;
pub mod config;
pub mod diag;
pub mod engine;
pub mod error;
pub mod externals;
pub mod flags;
pub mod inspect;
pub mod probe;
pub mod query;
pub mod report;
pub mod revision;
pub mod store;
pub mod synthq;

/// The Bazel workspace root containing `start`, if there is one.
///
/// Re-exported so a caller that only needs to know where the workspace is —
/// `cambi config explain` outside one, for instance — does not have to depend
/// on `cambium-bazel` for it.
pub fn bazel_workspace_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
    cambium_bazel::find_workspace_root(start)
}

pub use baseline::BaselineCache;
pub use config::Config;
pub use diag::{Code, Diagnostic, Diagnostics, Level, Location};
pub use engine::{AffectedRequest, BaseIndex, Baseline, BaselineOrigin, Engine, RunOutcome};
pub use error::{EngineError, Result};
pub use inspect::{IndexQuery, QueryOptions};
pub use probe::Probes;
pub use query::{BazelQuerier, PackageQuerier};
pub use report::{AffectedReport, AffectedTarget, DirectReason, Explanation, Stats, Timings};
pub use revision::{RevisionQuerier, RevisionWorkspace};
pub use synthq::SynthQuerier;
