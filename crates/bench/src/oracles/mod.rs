//! The three oracles, each named by what it proves.
//!
//! No oracle proves precision (Bazel has no query that says "and nothing else
//! could change") so each of these answers a soundness question and nothing
//! more (docs/METHODOLOGY.md).
//!
//! - [`rdeps`]: *did a tool miss a target?* `bazel query "kind(rule,
//!   rdeps(//..., set(<changed files>)))"` at head; cambium's answer must be a
//!   superset. Runs on the walk steps the tier's schedule samples.
//! - [`rebuild`]: *is the incremental updater exact?* At the end of a walk,
//!   a cold index built at the pin is diffed node by node, edge by edge and
//!   rule hash by rule hash against the index the walk carried there.
//! - [`load`]: *is the static `.bzl` load closure complete?* On a change set
//!   touching a `.bzl`, Bazel's `rbuildfiles` must be a subset of the
//!   packages cambium dirtied. No workload runs it since the walk replaced
//!   the change-set comparison.
//!
//! [`repo_inputs`] is not an oracle of its own: it reads Bazel's repository
//! marker files for the `rdeps` oracle's second root source.

pub mod load;
pub mod rdeps;
pub mod rebuild;
pub mod repo_inputs;
