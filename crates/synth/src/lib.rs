//! Synthetic Bazel graphs for cambium's tests and benchmarks.
//!
//! The generator is deterministic from a seed, produces graphs whose shape
//! matches what large monorepos look like (log-normal package sizes, hub
//! libraries, layered DAG), and can emit either index rows directly or a real
//! Bazel workspace so the same graph can be measured against `bazel query`.

pub mod graph;
pub mod spec;

pub use graph::{generate, Kind, SynthBzl, SynthGraph, SynthPackage, SynthTarget};
pub use spec::{Dist, GraphSpec};
