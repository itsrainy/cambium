//! Synthetic Bazel graphs for cambium's tests and benchmarks.
//!
//! The generator is deterministic from a seed, produces graphs whose shape
//! matches what large monorepos look like (log-normal package sizes, hub
//! libraries, layered DAG), and can emit either index rows directly or a real
//! Bazel workspace so the same graph can be measured against `bazel query`.

pub mod emit;
pub mod graph;
pub mod mutate;
pub mod proto;
pub mod reference;
pub mod sky;
pub mod spec;

pub use emit::{emit_git_history, emit_workspace, force_clear, EmitError, EmittedRepo, SynthFiles};
pub use graph::{generate, Kind, SynthBzl, SynthGraph, SynthPackage, SynthTarget};
pub use mutate::{apply_one, commit, ChangeSpec, Commit, Mutation, MutationWeights};
pub use reference::{deep_hashes, reference_affected};
pub use spec::{Dist, GraphSpec};
