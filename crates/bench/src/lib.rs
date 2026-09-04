//! Benchmark harness for cambium (spec 13).
//!
//! Every in-memory stage runs without Bazel, so scaling behaviour can be
//! measured at sizes no real repository reaches. The Bazel-backed end-to-end
//! comparison uses the emitted synthetic workspace, and includes the
//! "dump the whole graph at both commits" baseline the design replaces.

pub mod harness;
pub mod runner;
pub mod stages;

pub use harness::{BenchReport, Machine, StageResult};
pub use stages::{StageContext, IN_MEMORY_STAGES};
