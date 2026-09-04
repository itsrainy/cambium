//! cambium's orchestration layer.
//!
//! A run is: load a baseline index, ask git what changed, turn that into a set
//! of dirty packages, re-query only those with Bazel, splice their rows into
//! the index, derive the direct set, and walk the reverse edges. Every step is
//! proportional to the size of the change or the size of the answer — except
//! the splice memcpy, which is proportional to the graph but measured in tens
//! of milliseconds.

pub mod config;
pub mod engine;
pub mod error;
pub mod inspect;
pub mod query;
pub mod report;
pub mod store;
pub mod synthq;

pub use config::Config;
pub use engine::{AffectedRequest, Engine, RunOutcome};
pub use error::{EngineError, Result};
pub use inspect::{IndexQuery, QueryOptions};
pub use query::{BazelQuerier, PackageQuerier};
pub use report::{AffectedReport, DirectReason, Explanation, Stats, Timings};
pub use synthq::SynthQuerier;
