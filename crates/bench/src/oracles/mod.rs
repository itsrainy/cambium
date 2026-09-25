//! The three oracles, each named by what it proves.
//!
//! No oracle proves precision — Bazel has no query that says "and nothing else
//! could change" — so each of these answers a soundness question and nothing
//! more (docs/METHODOLOGY.md).
//!
//! - [`rdeps`] — *did a tool miss a target?* `bazel query "kind(rule,
//!   rdeps(//..., set(<changed files>)))"` at head; cambium's answer must be a
//!   superset. Runs on every incremental change set.
//! - [`rebuild`] — *is the incremental updater exact?* After a history run, a
//!   cold index built at the pin is diffed node by node, edge by edge and rule
//!   hash by rule hash against the index the history produced.
//! - [`load`] — *is the static `.bzl` load closure complete?* On every change
//!   set touching a `.bzl`, Bazel's `rbuildfiles` must be a subset of the
//!   packages cambium dirtied.
//!
//! [`repo_inputs`] is not an oracle of its own: it reads Bazel's repository
//! marker files for the `rdeps` oracle's second root source.

pub mod load;
pub mod rdeps;
pub mod rebuild;
pub mod repo_inputs;
