//! The cambium benchmark: what it runs on, what it runs, and what checks it.
//!
//! The vocabulary is [`docs/METHODOLOGY.md`](../../../docs/METHODOLOGY.md) and
//! this crate is its implementation. Four nouns, and every module is one of
//! them:
//!
//! - **Subject**: a Bazel repository a benchmark points at ([`subject`]),
//!   either a real one from `subject_repos.toml` ([`manifest`]) or a synthetic
//!   one emitted from a preset. Mirrors, worktrees, stores and the generated
//!   per-subject `cambium.toml` live in [`checkout`], under the subjects cache
//!   directory ([`checkout::SubjectsDir`]).
//! - **Tool**: anything that answers "which targets could this change
//!   affect": cambium itself, bazel-diff and Tango ([`tools`]).
//! - **Oracle**: an independent check named by what it proves
//!   ([`oracles`]): `rdeps` (did a tool miss a target?), `rebuild` (is the
//!   incremental updater exact?) and `load` (is the static `.bzl` load closure
//!   complete?). No workload runs `load` at present.
//! - **Workload**: what the tools are run on. There is one, the [`walk`]: it
//!   cold-builds an index at the start of a subject's first-parent window and
//!   carries it forward one commit at a time to the pin, asking cambium and
//!   the peers the question a CI job asks at each step and scoring every
//!   answer against the `rdeps` oracle. [`skyframe`] measures the Skyframe
//!   prototypes on the walk's own steps. [`classify`] names the kinds of
//!   disagreement, [`report`] renders `docs/RESULTS.md`, and [`state`] is
//!   what makes a killed run resumable.
//!
//! Beside the workloads sits [`labels`], which is not part of a run's
//! result: it is the label sets a walk step compared, written whole beside
//! `walk-<pin>.json` so a disagreement can be examined after
//! the fact rather than through the twenty examples the JSON has room for.
//!
//! Beside them sits [`progress`], which is not a result either: it is the one
//! file a run writes purely so that a reader can follow it (which step,
//! which commit or change set, which tool, since when) and what
//! `cambium-bench watch` polls. The harness is its only writer.
//!
//! [`watch`] is its reader: it turns the progress, state and result files into
//! one snapshot and renders the matrix, in two pure halves so the table is
//! checked against fixture files with no terminal anywhere.
//!
//! Beside them sits [`replay`], which is not a workload: it drives the
//! engine's static stages from a stored index and a list of changed paths, so
//! that an engine change can be checked against a recorded walk's own answers
//! offline, in milliseconds, before the benchmark machine is started.

pub mod checkout;
pub mod classify;
pub mod env;
pub mod harness;
pub mod labels;
pub mod manifest;
pub mod oracles;
pub mod plan;
pub mod precheck;
pub mod progress;
pub mod remote;
pub mod replay;
pub mod report;
pub mod run;
pub mod schema;
pub mod servers;
pub mod session;
pub mod skyframe;
pub mod state;
pub mod subject;
pub mod tools;
pub mod util;
pub mod walk;
pub mod watch;

// No root re-exports. Every one of these types is named by the module it
// belongs to -- `walk::WalkResult`, `classify::Class`,
// `tools::tango::Tango` -- and a second name for it at the crate root only
// hides which of the four nouns above a thing is.

/// The version stamped into every result file this crate writes.
///
/// It is the benchmark's own contract, not the engine's: the two used to share
/// `cambium_engine::report::SCHEMA_VERSION`, which meant a benchmark field could
/// not be renamed without claiming the `cambi affected` JSON had changed.
/// Bumped to 6 by the rename to the methodology's vocabulary (`pairs` became
/// `change_sets`, `producers` became `tools`) and to 7 by the one-walk
/// redesign (issue #14), which replaced the `history`/`changes` result shapes
/// with `walk-<pin>.json`. Old schema-6 files are not migrated; they stay in
/// place and this cambium refuses to read them.
pub const RESULT_SCHEMA: u32 = 7;

/// The window `--quick` sets: a bounded, deterministic slice that finishes on
/// a laptop. It is sugar for a flag, not a mode: the results land in the
/// same tree as any other run, and the size that produced them is inside the
/// result file.
pub const QUICK_COMMITS: usize = 50;

/// `bench-results/<subject>/`: the per-subject directory that holds
/// `state.json` and the cold indexes every workload shares.
pub fn subject_dir(root: &std::path::Path, subject: &str) -> std::path::PathBuf {
    root.join(subject)
}
